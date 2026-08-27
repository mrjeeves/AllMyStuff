//! AllMyStuff GUI — Tauri shell.
//!
//! The window is a Svelte app; this Rust side:
//!
//!  1. **Brings up the per-machine node** ([`ensure_node_running`]) — one
//!     `allmystuff-serve` node per machine, reused if the Always-On service
//!     already runs one, else spawned and tied to this app's lifetime. The node
//!     owns the live [`Mesh`](allmystuff_node::mesh::Mesh) and supervises the
//!     `myownmesh` daemon; the GUI no longer runs either in-process.
//!  2. **Drives that node over its control socket**
//!     ([`NodeClient`]) — every node-backed Tauri command is one short request,
//!     and the node's event stream is re-emitted onto Tauri's bus so the
//!     front-end sees exactly what it used to when the engine ran in-process.
//!  3. **Self-updates** via `allmystuff-updater` (its own release feed —
//!     not the daemon's).

#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

// A plain `cargo build --release` does not enable Tauri's production asset
// protocol. It still produces a plausible-looking executable, but that
// executable opens `devUrl` and leaves users staring at
// ERR_CONNECTION_REFUSED on localhost. Fail the build instead; `tauri build`
// enables `tauri/custom-protocol` and embeds `frontendDist` as intended.
#[cfg(all(not(debug_assertions), dev))]
compile_error!(
    "release GUI built in Tauri dev mode; use `pnpm tauri build` so frontendDist is embedded"
);

use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc,
    },
    time::{Duration, Instant, UNIX_EPOCH},
};

// The node engine lives in the `allmystuff-node` crate; this shell is a thin
// client of the per-machine node's control socket (see
// `allmystuff_node::node_control`), driving it rather than linking it in.
use allmystuff_graph::{Grant, Person};
#[cfg(all(windows, not(debug_assertions)))]
use allmystuff_node::node_control::running_node_satisfies;
use allmystuff_node::node_control::{ensure_node_running, NodeChild, NodeClient, NodeEvent};
use notify::Watcher as _;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tauri::{Emitter, Manager, RunEvent, State};
use tauri_plugin_autostart::ManagerExt;

mod backend_recovery;
#[allow(dead_code)] // parsers for the other desktop OSes are exercised by this module's tests
mod host_wifi;
#[cfg(windows)]
mod shell_icon;
mod window_behavior;

use backend_recovery::{
    BackendRecovery, Decision as RecoveryDecision, Observation as RecoveryObservation,
    Ownership as RecoveryOwnership, SocketState as RecoverySocketState, WEDGED_RESTART_ROUNDS,
};

#[derive(Default)]
struct OwnedNode {
    child: Option<NodeChild>,
    /// Monotonic identity for GUI-owned node processes. Reusing an external
    /// service node leaves this unchanged because the GUI does not own it.
    generation: u64,
}

impl OwnedNode {
    fn install(&mut self, child: NodeChild) -> u64 {
        self.generation = self.generation.saturating_add(1);
        self.child = Some(child);
        self.generation
    }

    fn is_alive(&mut self) -> bool {
        self.child.as_mut().is_some_and(NodeChild::is_alive)
    }

    fn observation(&mut self) -> (RecoveryOwnership, u64) {
        let ownership = if self.is_alive() {
            RecoveryOwnership::GuiOwnedAlive
        } else {
            RecoveryOwnership::NoLiveGuiChild
        };
        (ownership, self.generation)
    }

    fn take(&mut self) {
        self.child.take();
    }
}

struct LocalDirectoryWatch {
    _watcher: notify::RecommendedWatcher,
}

struct AppState {
    node: Arc<NodeClient>,
    /// The node we spawned, if Always-On wasn't already running one. Held so
    /// it's killed when the app exits (Always-On off => node lives only with
    /// the app); a reused service node has no child here and keeps running.
    node_child: Mutex<OwnedNode>,
    local_files: Arc<Mutex<LocalFileBrowser>>,
    local_directory_watchers: Arc<Mutex<HashMap<u64, LocalDirectoryWatch>>>,
    next_local_directory_watch: AtomicU64,
}

// ---- this machine -----------------------------------------------------

/// Scan this machine: `{ node_id, label, summary, capabilities }`. `node_id`
/// is the mesh device id once the session is up (so capabilities match what
/// peers see), else `"this"` for the offline/demo graph; `label` is the
/// hostname shown on the local node.
#[tauri::command]
async fn scan_self(state: State<'_, AppState>) -> Result<Value, String> {
    state
        .node
        .request("scan_self", json!({}))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn scan_full() -> Result<Value, String> {
    serde_json::to_value(allmystuff_inventory::scan()).map_err(|e| e.to_string())
}

// ---- live mesh (presence + routing + audio) ---------------------------

/// Offer a connection from one capability to another. Returns the route id.
/// `session` is the terminal multi-attach hook: `Some(id)` makes a terminal
/// Offer name an already-running host shell to attach to (shared, tmux-style),
/// `None` (and every non-terminal route) mints a fresh one.
#[tauri::command]
async fn connect_route(
    state: State<'_, AppState>,
    from: String,
    to: String,
    media: String,
    video: Option<Vec<String>>,
    session: Option<String>,
    room: Option<String>,
) -> Result<String, String> {
    let v = state
        .node
        .request(
            "connect_route",
            json!({ "from": from, "to": to, "media": media, "video": video, "session": session, "room": room }),
        )
        .await
        .map_err(|e| e.to_string())?;
    serde_json::from_value(v).map_err(|e| e.to_string())
}

#[tauri::command]
async fn disconnect_route(state: State<'_, AppState>, route_id: String) -> Result<(), String> {
    state
        .node
        .request("disconnect_route", json!({ "route_id": route_id }))
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
async fn drive_map(
    state: State<'_, AppState>,
    target: String,
    root: String,
    label: String,
    mount: String,
) -> Result<String, String> {
    let value = state
        .node
        .request(
            "drive_map",
            json!({ "target": target, "root": root, "label": label, "mount": mount }),
        )
        .await
        .map_err(|error| error.to_string())?;
    serde_json::from_value(value).map_err(|error| error.to_string())
}

#[tauri::command]
async fn drive_map_from(
    state: State<'_, AppState>,
    source: String,
    root: String,
    label: String,
    mount: String,
) -> Result<(), String> {
    state
        .node
        .request(
            "drive_map_from",
            json!({ "source": source, "root": root, "label": label, "mount": mount }),
        )
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[tauri::command]
async fn drive_mappings(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    state
        .node
        .request("drive_mappings", json!({}))
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn drive_unmap(
    state: State<'_, AppState>,
    mapping: String,
    source: Option<String>,
    target: Option<String>,
) -> Result<(), String> {
    state
        .node
        .request(
            "drive_unmap",
            json!({ "mapping": mapping, "source": source, "target": target }),
        )
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// Share a folder on `source` — whichever machine of mine holds it — and
/// return the id it minted. The share builder pins its grant to that id, so
/// the path stays on the machine that owns the disk.
#[tauri::command]
async fn folder_share_from(
    state: State<'_, AppState>,
    source: String,
    path: String,
    label: String,
) -> Result<serde_json::Value, String> {
    state
        .node
        .request(
            "folder_share_from",
            json!({ "source": source, "path": path, "label": label }),
        )
        .await
        .map_err(|error| error.to_string())
}

/// Open a folder someone shared with us as a native drive here, at our own
/// choice of mount point.
#[tauri::command]
async fn folder_open(
    state: State<'_, AppState>,
    source: String,
    folder: String,
    mount: String,
) -> Result<(), String> {
    state
        .node
        .request(
            "folder_open",
            json!({ "source": source, "folder": folder, "mount": mount }),
        )
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// Ask one of our own fleet machines to open an opaque folder share directly
/// from its original source. The GUI coordinates the endpoints; it never
/// learns the source path or proxies the drive's bytes.
#[tauri::command]
async fn folder_open_on(
    state: State<'_, AppState>,
    target: String,
    source: String,
    folder: String,
    mount: String,
) -> Result<(), String> {
    state
        .node
        .request(
            "folder_open_on",
            json!({ "target": target, "source": source, "folder": folder, "mount": mount }),
        )
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[tauri::command]
async fn kvm_media_stage(
    state: State<'_, AppState>,
    source: String,
    kvm: String,
    path: String,
    label: String,
) -> Result<(), String> {
    state
        .node
        .request(
            "kvm_media_stage",
            json!({ "source": source, "kvm": kvm, "path": path, "label": label }),
        )
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[tauri::command]
async fn kvm_media_unmount(state: State<'_, AppState>, kvm: String) -> Result<(), String> {
    state
        .node
        .request("kvm_media_unmount", json!({ "kvm": kvm }))
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
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
async fn claim_node(state: State<'_, AppState>, node: String) -> Result<(), String> {
    state
        .node
        .request("claim_node", json!({ "node": node }))
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Ask one of your fleet machines to update its AllMyStuff to the channel's
/// latest release and restart. The target enforces owner/fleet before acting;
/// its next presence advert (the new version) confirms it landed.
#[tauri::command]
async fn upgrade_node(state: State<'_, AppState>, node: String) -> Result<(), String> {
    state
        .node
        .request("upgrade_node", json!({ "node": node }))
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Ask one of your fleet machines to **restart** its AllMyStuff app (relaunch
/// onto the same build — no update). Owner/fleet enforced on the far side; its
/// next presence advert is the confirmation.
#[tauri::command]
async fn restart_node(state: State<'_, AppState>, node: String) -> Result<(), String> {
    state
        .node
        .request("restart_node", json!({ "node": node }))
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Restart **this** machine's AllMyStuff app right now — the local twin of
/// [`restart_node`], for the gear menu's "Restart app" on your own device.
/// Tauri relaunches the window (and the supervised node child comes back with
/// it). Never returns.
#[tauri::command]
fn restart_app(app: tauri::AppHandle) {
    app.restart()
}

/// Reboot a machine's whole OS — the gear menu's step past [`restart_node`].
/// The node routes it: our own device hands straight to the OS, a fleet
/// machine is asked over the mesh (owner/fleet enforced there, and the OS's
/// own privilege rules after that). Its presence dropping and returning is
/// the confirmation.
#[tauri::command]
async fn restart_device(state: State<'_, AppState>, node: String) -> Result<(), String> {
    state
        .node
        .request("restart_device", json!({ "node": node }))
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Re-learn a node's details for the refresh control. `node` omitted = **this**
/// device (re-scan + re-advertise its own profile); a peer id asks that node to
/// re-send its profile (rate-limited on the far side) so our stored view of its
/// UI/options/shares is refreshed. Best-effort; the next presence is the proof.
#[tauri::command]
async fn refresh_node(state: State<'_, AppState>, node: Option<String>) -> Result<(), String> {
    let arg = match node {
        Some(node) => json!({ "node": node }),
        None => json!({}),
    };
    state
        .node
        .request("refresh_node", arg)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Put this device into / out of claim mode so another of your machines can
/// adopt it. Returns whether it's now claimable.
#[tauri::command]
async fn set_claimable(state: State<'_, AppState>, claimable: bool) -> Result<bool, String> {
    let v = state
        .node
        .request("set_claimable", json!({ "claimable": claimable }))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::from_value(v).map_err(|e| e.to_string())
}

/// Flip **this device's** claims-over-the-public-mesh setting (strictly
/// device-local — never fleet-synced, never remotely settable). Returns the
/// new value.
#[tauri::command]
async fn set_public_claims(state: State<'_, AppState>, on: bool) -> Result<bool, String> {
    let v = state
        .node
        .request("set_public_claims", json!({ "on": on }))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::from_value(v).map_err(|e| e.to_string())
}

/// Claim a remote device by the claim code its operator read off it. Joins
/// the code's randomized rendezvous, claims, and tears it down again.
#[tauri::command]
async fn claim_via_code(state: State<'_, AppState>, code: String) -> Result<(), String> {
    state
        .node
        .request("claim_via_code", json!({ "code": code }))
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Point a KVM appliance (`node`) at the machine it controls (`target`). The
/// KVM enforces owner/fleet before applying, then re-advertises its new
/// binding — that presence is the confirmation, exactly as a claim confirms.
#[tauri::command]
async fn kvm_attach(
    state: State<'_, AppState>,
    node: String,
    target: String,
) -> Result<(), String> {
    state
        .node
        .request("kvm_attach", json!({ "node": node, "target": target }))
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Clear a KVM appliance's binding — it no longer represents any machine. Same
/// owner/fleet enforcement + presence confirmation as [`kvm_attach`].
#[tauri::command]
async fn kvm_detach(state: State<'_, AppState>, node: String) -> Result<(), String> {
    state
        .node
        .request("kvm_detach", json!({ "node": node }))
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Walk a KVM appliance onto another mesh — the fleet owner's membership
/// tool. The KVM validates, refuses its own fleet mesh, joins, and
/// re-advertises its membership list — that presence is the confirmation.
#[tauri::command]
async fn kvm_mesh_add(
    state: State<'_, AppState>,
    node: String,
    network_id: String,
) -> Result<(), String> {
    state
        .node
        .request(
            "kvm_mesh_add",
            json!({ "node": node, "network_id": network_id }),
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Take a KVM appliance off a mesh (never its fleet mesh). Same enforcement
/// + presence confirmation as [`kvm_mesh_add`].
#[tauri::command]
async fn kvm_mesh_remove(
    state: State<'_, AppState>,
    node: String,
    network_id: String,
) -> Result<(), String> {
    state
        .node
        .request(
            "kvm_mesh_remove",
            json!({ "node": node, "network_id": network_id }),
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Persist an outbound grant to a person — what they may do with my stuff —
/// so it survives a restart. The GUI resolves the person and the node the
/// grant is recorded against; the node is the durable source of truth and the
/// next snapshot reflects it.
#[tauri::command]
async fn share_grant(
    state: State<'_, AppState>,
    person: Person,
    node: String,
    grant: Grant,
) -> Result<(), String> {
    state
        .node
        .request(
            "share_grant",
            json!({ "person": person, "node": node, "grant": grant }),
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Revoke a grant by its (content-derived) id from a person's durable share,
/// and tell their devices to drop it too.
#[tauri::command]
async fn share_revoke(
    state: State<'_, AppState>,
    person: String,
    grant_id: String,
) -> Result<(), String> {
    state
        .node
        .request(
            "share_revoke",
            json!({ "person": person, "grant_id": grant_id }),
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Stop sharing with a person entirely — drop the whole durable record and
/// revoke each grant on their devices.
#[tauri::command]
async fn share_stop(state: State<'_, AppState>, person: String) -> Result<(), String> {
    state
        .node
        .request("share_stop", json!({ "person": person }))
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Forward one keyboard/mouse event down an active outbound input route —
/// the console window's control stream.
#[tauri::command]
async fn send_input(
    state: State<'_, AppState>,
    route_id: String,
    action: serde_json::Value,
) -> Result<(), String> {
    state
        .node
        .request(
            "send_input",
            json!({ "route_id": route_id, "action": action }),
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Read this machine's clipboard and push it down an active outbound
/// clipboard route — the console calls this the moment it forwards a paste.
/// The backend does the read (the only place that can see file references on
/// the OS clipboard) and streams text, an image, or files.
#[tauri::command]
async fn clipboard_paste(state: State<'_, AppState>, route_id: String) -> Result<(), String> {
    state
        .node
        .request("clipboard_paste", json!({ "route_id": route_id }))
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Stream files selected by the OS-native drag/drop event down the live
/// clipboard route. The webview supplies trusted local paths; the node opens
/// and chunks them, so the GUI never loads a whole file into JavaScript.
#[tauri::command]
async fn clipboard_drop(
    state: State<'_, AppState>,
    route_id: String,
    paths: Vec<String>,
) -> Result<(), String> {
    state
        .node
        .request(
            "clipboard_drop",
            json!({ "route_id": route_id, "paths": paths }),
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Copy/cut **from** the remote: ask the far end to read its clipboard now and
/// send it back down the route, so the selection it just copied lands on this
/// machine. The console calls this right after forwarding the copy/cut
/// keystroke; the backend opens the acceptance window and fires the request.
#[tauri::command]
async fn clipboard_pull(state: State<'_, AppState>, route_id: String) -> Result<(), String> {
    state
        .node
        .request("clipboard_pull", json!({ "route_id": route_id }))
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
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
async fn video_watch(
    app: tauri::AppHandle,
    route_id: String,
    decode: Option<bool>,
    decoder: Option<String>,
) -> u64 {
    let state = app.state::<AppState>();
    match state
        .node
        .request(
            "video_watch",
            json!({ "route_id": route_id, "decode": decode, "decoder": decoder }),
        )
        .await
    {
        Ok(v) => serde_json::from_value(v).unwrap_or_default(),
        Err(e) => {
            tracing::warn!("video_watch failed: {e:#}");
            0
        }
    }
}

/// Drain the queued packets for a route as one raw batch:
/// `[u32 len][28-byte header + payload]…`, empty when nothing arrived.
#[tauri::command]
async fn video_poll(app: tauri::AppHandle, route_id: String) -> tauri::ipc::Response {
    let state = app.state::<AppState>();
    tauri::ipc::Response::new(
        state
            .node
            .request_bytes("video_poll", json!({ "route_id": route_id }))
            .await
            .unwrap_or_default(),
    )
}

/// Stop streaming a route's frames to the front-end (console closed or
/// switched input). The token scopes the release to the claim that made
/// it, so a late unwatch can't tear down a newer watcher of the same
/// route. Idempotent.
#[tauri::command]
async fn video_unwatch(app: tauri::AppHandle, route_id: String, token: u64) {
    let state = app.state::<AppState>();
    if let Err(e) = state
        .node
        .request(
            "video_unwatch",
            json!({ "route_id": route_id, "token": token }),
        )
        .await
    {
        tracing::warn!("video_unwatch failed: {e:#}");
    }
}

/// Ask the sender of an inbound display route for a clean decode entry
/// (IDR) now — the console's decoder hit an error. Rate-limited backend-
/// side; safe to call from a decode-error handler.
#[tauri::command]
async fn video_refresh(state: State<'_, AppState>, route_id: String) -> Result<(), String> {
    state
        .node
        .request("video_refresh", json!({ "route_id": route_id }))
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Report the console's decode health for an inbound display route back to its
/// streamer (receiver → sender), so the streamer can adapt the stream. Sent
/// periodically by the console; best-effort, an old streamer drops it.
#[tauri::command]
async fn video_feedback(
    state: State<'_, AppState>,
    route_id: String,
    recv_fps: u32,
    decode_fails: u32,
    queue_depth: u32,
) -> Result<(), String> {
    state
        .node
        .request(
            "video_feedback",
            json!({
                "route_id": route_id,
                "recv_fps": recv_fps,
                "decode_fails": decode_fails,
                "queue_depth": queue_depth,
            }),
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Ask the sender of an inbound display route to stream with these
/// quality picks; absent values mean "automatic". The console's pills.
#[tauri::command]
async fn tune_route(
    state: State<'_, AppState>,
    route_id: String,
    max_edge: Option<u32>,
    bitrate: Option<u32>,
    fps: Option<u32>,
    game: Option<bool>,
    mode: Option<String>,
) -> Result<(), String> {
    state
        .node
        .request(
            "tune_route",
            json!({ "route_id": route_id, "max_edge": max_edge, "bitrate": bitrate, "fps": fps, "game": game, "mode": mode }),
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// The effective encode dials for a route THIS machine is streaming — the
/// "requested → effective" panel reads it ~1 Hz while open. Returns JSON
/// null when this machine isn't the streamer (the ordinary remote-view
/// case), so the viewer falls back to its own measured actuals. GUI-internal
/// and read-only — no wire traffic.
#[tauri::command]
async fn route_dials(
    state: State<'_, AppState>,
    route_id: String,
) -> Result<serde_json::Value, String> {
    state
        .node
        .request("route_dials", json!({ "route_id": route_id }))
        .await
        .map_err(|e| e.to_string())
}

/// Flip the Experimental (Labs) tier — or one feature — on this node.
/// GUI-internal, never wire-visible; the Mode dropdown's toggle.
#[tauri::command]
async fn labs_set(
    state: State<'_, AppState>,
    on: bool,
    feature: Option<String>,
) -> Result<serde_json::Value, String> {
    state
        .node
        .request("labs_set", json!({ "on": on, "feature": feature }))
        .await
        .map_err(|e| e.to_string())
}

// ---- terminal (the mesh-native shell) ----------------------------------

/// Forward keystrokes or a resize from a terminal window down its active
/// terminal route (the viewer side of a mesh-native shell).
#[tauri::command]
async fn term_send(
    state: State<'_, AppState>,
    route_id: String,
    event: serde_json::Value,
) -> Result<(), String> {
    state
        .node
        .request("term_send", json!({ "route_id": route_id, "event": event }))
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Register the calling terminal window's interest in a route's output.
/// Bytes buffer backend-side from route-activation (so the shell's first
/// prompt is never lost); the window drains them with `term_poll` on each
/// `allmystuff://term-ready` poke. Same pull-not-push shape as video.
#[tauri::command]
async fn term_watch(app: tauri::AppHandle, route_id: String) -> u64 {
    let state = app.state::<AppState>();
    match state
        .node
        .request("term_watch", json!({ "route_id": route_id }))
        .await
    {
        Ok(v) => serde_json::from_value(v).unwrap_or_default(),
        Err(e) => {
            tracing::warn!("term_watch failed: {e:#}");
            0
        }
    }
}

/// Drain the queued output for a terminal route as one raw batch:
/// `[u32 le len][bytes]…`, empty when nothing arrived.
#[tauri::command]
async fn term_poll(app: tauri::AppHandle, route_id: String) -> tauri::ipc::Response {
    let state = app.state::<AppState>();
    tauri::ipc::Response::new(
        state
            .node
            .request_bytes("term_poll", json!({ "route_id": route_id }))
            .await
            .unwrap_or_default(),
    )
}

/// Release a terminal window's claim on a route's output (tab closed).
/// Token-scoped and idempotent, like `video_unwatch`.
#[tauri::command]
async fn term_unwatch(app: tauri::AppHandle, route_id: String, token: u64) {
    let state = app.state::<AppState>();
    if let Err(e) = state
        .node
        .request(
            "term_unwatch",
            json!({ "route_id": route_id, "token": token }),
        )
        .await
    {
        tracing::warn!("term_unwatch failed: {e:#}");
    }
}

/// Ask `node` for its open terminal sessions (the picker's "attach to an
/// existing shell" list). The **local** machine answers synchronously —
/// the returned list is its own open shells; a **remote** host answers
/// asynchronously, returning `null` here while the reply arrives as an
/// `allmystuff://terminal-sessions` event. Owner/fleet gated both ends.
#[tauri::command]
async fn terminal_sessions(
    state: State<'_, AppState>,
    node: String,
) -> Result<Option<Vec<allmystuff_protocol::TerminalSessionInfo>>, String> {
    let v = state
        .node
        .request("terminal_sessions", json!({ "node": node }))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::from_value(v).map_err(|e| e.to_string())
}

/// Open a secondary app window (terminal / files / console / room / video) —
/// or focus the existing one with this `label` — and stamp the freshly built
/// window with its own taskbar identity. **Every** secondary window is built
/// through here so the identity step can't be forgotten: `aumid` is a required
/// argument (see [`set_taskbar_identity`]), applied to each new window at
/// creation. A future window kind just calls this with its own `AUMID_*`.
fn open_secondary_window(
    app: &tauri::AppHandle,
    label: &str,
    url: String,
    title: &str,
    inner_size: (f64, f64),
    min_inner_size: (f64, f64),
    aumid: &'static str,
) -> Result<(), String> {
    if let Some(existing) = app.get_webview_window(label) {
        // A secondary window can outlive its native HWND in Tauri's window
        // registry after a renderer or WebView failure. `set_focus()` on that
        // orphan returns an error; ignoring it made every later click look
        // successful while no window appeared. Probe the native window, make
        // a healthy one visible again, and discard a stale entry so the build
        // below can recreate it under the same stable label.
        match (existing.is_visible(), existing.is_minimized()) {
            (Ok(_), Ok(minimized)) => {
                let shown = existing.show();
                let restored = if minimized {
                    existing.unminimize()
                } else {
                    Ok(())
                };
                if shown.is_ok() && restored.is_ok() {
                    // Windows may refuse a foreground-stealing focus request;
                    // the window is still visible, so that is not a launch
                    // failure and must not make us destroy it.
                    let _ = existing.set_focus();
                    return Ok(());
                }
                tracing::warn!(
                    window = label,
                    show_error = ?shown.err(),
                    restore_error = ?restored.err(),
                    "secondary window could not be restored; recreating it"
                );
            }
            (visible, minimized) => {
                tracing::warn!(
                    window = label,
                    visible_error = ?visible.err(),
                    minimized_error = ?minimized.err(),
                    "secondary window registry entry is stale; recreating it"
                );
            }
        }
        let _ = existing.destroy();
    }
    let window = tauri::WebviewWindowBuilder::new(app, label, tauri::WebviewUrl::App(url.into()))
        .title(title)
        .inner_size(inner_size.0, inner_size.1)
        .min_inner_size(min_inner_size.0, min_inner_size.1)
        .visible(true)
        .build()
        .map_err(|e| e.to_string())?;
    window.show().map_err(|e| e.to_string())?;
    let _ = window.set_focus();
    set_taskbar_identity(app, label, aumid);
    Ok(())
}

/// Open (or focus) the dedicated terminal window for `node` — one OS
/// window per machine, holding that machine's terminal tabs. The window
/// loads the same app with `?terminal=<node>`.
#[tauri::command]
async fn open_terminal_window(
    app: tauri::AppHandle,
    node: String,
    attach: Option<String>,
) -> Result<(), String> {
    // A plain terminal window is one-per-machine (`terminal-<node>`); a
    // *popped-out* tab attaches to a specific shared session and gets its own
    // window keyed by that session (`terminal-<node>-<session>`), so two
    // pop-outs never collide and re-popping the same shell just refocuses it.
    let (label, url) = match &attach {
        Some(session) => (
            format!("terminal-{}-{}", window_slug(&node), window_slug(session)),
            format!(
                "index.html?terminal={node}&attach={}",
                query_encode(session)
            ),
        ),
        None => (
            format!("terminal-{}", window_slug(&node)),
            format!("index.html?terminal={node}"),
        ),
    };
    open_secondary_window(
        &app,
        &label,
        url,
        "AllMyStuff terminal",
        (940.0, 600.0),
        (480.0, 320.0),
        AUMID_TERMINAL,
    )
}

// ---- files (the mesh-native file manager) -------------------------------

#[tauri::command]
async fn files_namespace_adopt(
    state: State<'_, AppState>,
    parent: String,
    observations: Vec<Value>,
) -> Result<Value, String> {
    state
        .node
        .request(
            "files_namespace_adopt",
            json!({ "parent": parent, "observations": observations }),
        )
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn files_namespace_mutate(
    state: State<'_, AppState>,
    request: Value,
) -> Result<Value, String> {
    state
        .node
        .request("files_namespace_mutate", json!({ "request": request }))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn files_namespace_list(
    state: State<'_, AppState>,
    parent: String,
    cursor: Option<String>,
    limit: usize,
    expected_directory_version: Option<i64>,
) -> Result<Value, String> {
    state
        .node
        .request(
            "files_namespace_list",
            json!({
                "parent": parent,
                "cursor": cursor,
                "limit": limit,
                "expected_directory_version": expected_directory_version,
            }),
        )
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn files_canvas_snapshot(state: State<'_, AppState>) -> Result<Value, String> {
    state
        .node
        .request("files_canvas_snapshot", json!({}))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn files_canvas_status(state: State<'_, AppState>) -> Result<Value, String> {
    state
        .node
        .request("files_canvas_status", json!({}))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn files_canvas_apply(
    state: State<'_, AppState>,
    mutations: Vec<Value>,
) -> Result<Value, String> {
    state
        .node
        .request("files_canvas_apply", json!({ "mutations": mutations }))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn files_canvas_purge_tombstones(state: State<'_, AppState>) -> Result<Value, String> {
    state
        .node
        .request("files_canvas_purge_tombstones", json!({}))
        .await
        .map_err(|e| e.to_string())
}

#[derive(serde::Serialize)]
struct LocalFileLocation {
    id: String,
    label: String,
    path: String,
    kind: String,
}

#[cfg(windows)]
fn windows_file_is_hidden(attributes: u32) -> bool {
    use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_SYSTEM};
    attributes & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM) != 0
}

fn windows_shell_link_name(name: &str) -> bool {
    std::path::Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("lnk") || extension.eq_ignore_ascii_case("url")
        })
}

#[derive(serde::Serialize)]
struct LocalFileEntry {
    id: String,
    name: String,
    path: String,
    dir: bool,
    size: u64,
    modified: Option<u64>,
    hidden: bool,
    symlink: bool,
    #[serde(rename = "virtualItem")]
    virtual_item: bool,
    #[serde(rename = "shellIcon")]
    shell_icon: Option<String>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct LocalFileListing {
    id: String,
    path: String,
    platform: String,
    entries: Vec<LocalFileEntry>,
    next_cursor: Option<String>,
    complete: bool,
}

#[derive(Default)]
struct LocalFileBrowser {
    next_cursor: u64,
    cursors: HashMap<String, LocalFileCursor>,
}

struct LocalFileCursor {
    id: String,
    path: PathBuf,
    readers: VecDeque<std::fs::ReadDir>,
    synthetic: VecDeque<LocalFileEntry>,
    pending: Option<LocalFileEntry>,
    seen_ids: HashMap<String, usize>,
    seen_names: HashSet<String>,
    merge_names: bool,
    touched: Instant,
}

fn next_local_dir_entry(current: &mut LocalFileCursor) -> Option<std::fs::DirEntry> {
    loop {
        match current.readers.front_mut()?.next() {
            Some(Ok(item)) => {
                if current.merge_names {
                    let key = item.file_name().to_string_lossy().to_lowercase();
                    if !current.seen_names.insert(key) {
                        continue;
                    }
                }
                return Some(item);
            }
            Some(Err(_)) => continue,
            None => {
                current.readers.pop_front();
            }
        }
    }
}

#[cfg(windows)]
const RECYCLE_BIN_PARSE_NAME: &str = "::{645FF040-5081-101B-9F08-00AA002F954E}";

#[cfg(windows)]
fn recycle_bin_entry() -> LocalFileEntry {
    LocalFileEntry {
        id: "windows-shell:recycle-bin".into(),
        name: "Recycle Bin".into(),
        path: RECYCLE_BIN_PARSE_NAME.into(),
        dir: true,
        size: 0,
        modified: None,
        hidden: false,
        symlink: false,
        shell_icon: shell_icon::recycle_bin_icon(),
        virtual_item: true,
    }
}

#[cfg(windows)]
fn windows_desktop_parts(
    canonical: &Path,
) -> (Option<std::fs::ReadDir>, VecDeque<LocalFileEntry>, bool) {
    let is_desktop = dirs::desktop_dir()
        .and_then(|path| path.canonicalize().ok())
        .is_some_and(|desktop| desktop == canonical);
    if !is_desktop {
        return (None, VecDeque::new(), false);
    }
    let public_path = std::env::var_os("PUBLIC")
        .map(PathBuf::from)
        .map(|path| path.join("Desktop"))
        .and_then(|path| path.canonicalize().ok())
        .filter(|path| path != canonical);
    let public_reader = public_path.and_then(|path| std::fs::read_dir(path).ok());
    (public_reader, VecDeque::from([recycle_bin_entry()]), true)
}

#[cfg(windows)]
fn windows_desktop_watch_path(canonical: &Path) -> Option<PathBuf> {
    let is_desktop = dirs::desktop_dir()
        .and_then(|path| path.canonicalize().ok())
        .is_some_and(|desktop| desktop == canonical);
    is_desktop
        .then(|| {
            std::env::var_os("PUBLIC")
                .map(PathBuf::from)
                .map(|path| path.join("Desktop"))
                .and_then(|path| path.canonicalize().ok())
                .filter(|path| path != canonical)
        })
        .flatten()
}

#[cfg(not(windows))]
fn windows_desktop_watch_path(_canonical: &Path) -> Option<PathBuf> {
    None
}

#[cfg(not(windows))]
fn windows_desktop_parts(
    _canonical: &Path,
) -> (Option<std::fs::ReadDir>, VecDeque<LocalFileEntry>, bool) {
    (None, VecDeque::new(), false)
}

#[derive(serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum LocalPreview {
    Text { text: String },
    Image { mime: String, data: String },
    Unsupported,
}

fn local_path_for_display(path: &Path) -> String {
    #[cfg(windows)]
    let shown = shell_icon::shell_compatible_path(path);
    #[cfg(not(windows))]
    let shown = path.to_path_buf();
    shown.to_string_lossy().into_owned()
}

fn path_fallback_id(kind: &str, path: &Path, meta: &std::fs::Metadata, fold_case: bool) -> String {
    let shown = path.to_string_lossy();
    let normalized = if fold_case {
        shown.to_lowercase()
    } else {
        shown.into_owned()
    };
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in normalized.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let created = meta
        .created()
        .ok()
        .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
        .map(|at| at.as_nanos())
        .unwrap_or_default();
    format!("path:{kind}:{hash:016x}:{created:x}")
}

fn local_file_id(path: &Path, meta: &std::fs::Metadata, symlink: bool) -> String {
    #[cfg(not(windows))]
    let _ = symlink;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        return format!("unix:{}:{}", meta.dev(), meta.ino());
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt as _;
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        };
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        wide.push(0);
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS
                    | if symlink {
                        FILE_FLAG_OPEN_REPARSE_POINT
                    } else {
                        0
                    },
                std::ptr::null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
            let ok = unsafe { GetFileInformationByHandle(handle, &mut info) } != 0;
            unsafe { CloseHandle(handle) };
            if ok {
                let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
                return format!("windows:{:x}:{index:x}", info.dwVolumeSerialNumber);
            }
        }
        if symlink {
            return path_fallback_id("link", path, meta, true);
        }
        if let Ok(real) = path.canonicalize() {
            return path_fallback_id("canonical", &real, meta, true);
        }
        // Last resort for a provider that refuses both a stable handle and
        // canonicalization. Keep the path out of fleet metadata and include
        // creation time when the provider exposes it.
        return path_fallback_id("entry", path, meta, true);
    }
    #[allow(unreachable_code)]
    path_fallback_id("entry", path, meta, false)
}

fn local_file_entry(path: &Path) -> Result<LocalFileEntry, String> {
    let name = path
        .file_name()
        .ok_or("that item has no file name")?
        .to_string_lossy()
        .into_owned();
    let identity_meta = std::fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    let symlink = identity_meta.file_type().is_symlink();
    let target_meta = if symlink {
        std::fs::metadata(path).ok()
    } else {
        Some(identity_meta.clone())
    };
    let display_meta = target_meta.as_ref().unwrap_or(&identity_meta);
    #[cfg(windows)]
    let hidden = {
        use std::os::windows::fs::MetadataExt as _;
        name.starts_with('.') || windows_file_is_hidden(identity_meta.file_attributes())
    };
    #[cfg(not(windows))]
    let hidden = name.starts_with('.');
    #[cfg(windows)]
    let shell_icon = windows_shell_link_name(&name)
        .then(|| shell_icon::shortcut_icon(path))
        .flatten();
    #[cfg(not(windows))]
    let shell_icon = None;

    Ok(LocalFileEntry {
        id: local_file_id(path, &identity_meta, symlink),
        name,
        path: local_path_for_display(path),
        dir: display_meta.is_dir(),
        size: if display_meta.is_file() {
            display_meta.len()
        } else {
            0
        },
        modified: display_meta
            .modified()
            .ok()
            .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs()),
        hidden,
        shell_icon,
        symlink,
        virtual_item: false,
    })
}

fn location(id: &str, label: &str, path: Option<PathBuf>, kind: &str) -> Option<LocalFileLocation> {
    let path = path?;
    if !path.exists() {
        return None;
    }
    Some(LocalFileLocation {
        id: id.into(),
        label: label.into(),
        path: local_path_for_display(&path),
        kind: kind.into(),
    })
}

#[cfg(windows)]
fn windows_logical_drive_roots() -> Vec<String> {
    use windows_sys::Win32::Storage::FileSystem::GetLogicalDriveStringsW;
    let required = unsafe { GetLogicalDriveStringsW(0, std::ptr::null_mut()) };
    if required == 0 {
        return Vec::new();
    }
    let mut buffer = vec![0u16; required as usize + 1];
    let written = unsafe { GetLogicalDriveStringsW(buffer.len() as u32, buffer.as_mut_ptr()) };
    if written == 0 || written as usize >= buffer.len() {
        return Vec::new();
    }
    let mut roots = Vec::new();
    let mut start = 0usize;
    for index in 0..=written as usize {
        if buffer[index] != 0 {
            continue;
        }
        if index == start {
            break;
        }
        roots.push(String::from_utf16_lossy(&buffer[start..index]));
        start = index + 1;
    }
    roots
}

/// Native places for the Files sidebar. Reading these paths is local-only and
/// side-effect free; no mesh traffic is generated by browsing.
#[tauri::command]
fn local_file_locations() -> Vec<LocalFileLocation> {
    let mut out = Vec::new();
    out.extend(
        [
            location("home", "Home", dirs::home_dir(), "favorite"),
            location("desktop", "Desktop", dirs::desktop_dir(), "favorite"),
            location("documents", "Documents", dirs::document_dir(), "favorite"),
            location("downloads", "Downloads", dirs::download_dir(), "favorite"),
            location("pictures", "Pictures", dirs::picture_dir(), "favorite"),
        ]
        .into_iter()
        .flatten(),
    );
    for (index, volume) in allmystuff_inventory::scan().storage.into_iter().enumerate() {
        let Some(path) = volume.mount_point.map(PathBuf::from) else {
            continue;
        };
        let shown = if volume.name.trim().is_empty() {
            path.to_string_lossy().into_owned()
        } else {
            volume.name
        };
        if out.iter().any(|item| item.path == path.to_string_lossy()) {
            continue;
        }
        if let Some(item) = location(&format!("volume-{index}"), &shown, Some(path), "volume") {
            out.push(item);
        }
    }
    #[cfg(windows)]
    for root in windows_logical_drive_roots() {
        let normalized = root.trim_end_matches(['\\', '/']);
        if out.iter().any(|item| {
            item.path
                .trim_end_matches(['\\', '/'])
                .eq_ignore_ascii_case(normalized)
        }) {
            continue;
        }
        out.push(LocalFileLocation {
            id: format!(
                "volume-windows-{}",
                normalized.replace(':', "").to_ascii_lowercase()
            ),
            label: normalized.to_string(),
            path: root,
            kind: "volume".into(),
        });
    }
    out
}

#[tauri::command]
async fn local_file_list(
    state: State<'_, AppState>,
    path: String,
    cursor: Option<String>,
) -> Result<LocalFileListing, String> {
    const PAGE_SIZE: usize = 256;
    const CURSOR_TTL: Duration = Duration::from_secs(120);
    const MAX_CURSORS: usize = 8;
    let browser = state.local_files.clone();
    tokio::task::spawn_blocking(move || {
        let now = Instant::now();
        let mut current = if let Some(token) = cursor {
            let mut browser = browser.lock();
            browser
                .cursors
                .retain(|_, value| now.duration_since(value.touched) <= CURSOR_TTL);
            let current = browser
                .cursors
                .remove(&token)
                .ok_or_else(|| "that folder page expired; refresh it".to_string())?;
            let requested = PathBuf::from(&path)
                .canonicalize()
                .map_err(|error| error.to_string())?;
            if requested != current.path {
                return Err("that folder page belongs to another location".into());
            }
            current
        } else {
            let canonical = PathBuf::from(&path)
                .canonicalize()
                .map_err(|e| e.to_string())?;
            if !canonical.is_dir() {
                return Err("that location is not a folder".into());
            }
            let directory_meta = std::fs::metadata(&canonical).map_err(|e| e.to_string())?;
            let (public_reader, synthetic, merge_names) = windows_desktop_parts(&canonical);
            let mut readers = VecDeque::new();
            readers.push_back(std::fs::read_dir(&canonical).map_err(|e| e.to_string())?);
            if let Some(reader) = public_reader {
                readers.push_back(reader);
            }
            LocalFileCursor {
                id: local_file_id(&canonical, &directory_meta, false),
                path: canonical,
                readers,
                synthetic,
                pending: None,
                seen_ids: HashMap::new(),
                seen_names: HashSet::new(),
                merge_names,
                touched: now,
            }
        };

        let convert = |item: std::fs::DirEntry,
                       seen_ids: &mut HashMap<String, usize>|
         -> Option<LocalFileEntry> {
            let mut entry = local_file_entry(&item.path()).ok()?;
            let base_id = entry.id.clone();
            let count = seen_ids.entry(base_id.clone()).or_insert(0);
            *count += 1;
            if *count > 1 {
                entry.id = format!("{base_id}:entry:{}", entry.name);
            }
            Some(entry)
        };

        let mut entries = Vec::with_capacity(PAGE_SIZE);
        if let Some(entry) = current.pending.take() {
            entries.push(entry);
        }
        while entries.len() < PAGE_SIZE {
            if let Some(entry) = current.synthetic.pop_front() {
                entries.push(entry);
                continue;
            }
            let Some(item) = next_local_dir_entry(&mut current) else {
                break;
            };
            if let Some(entry) = convert(item, &mut current.seen_ids) {
                entries.push(entry);
            }
        }
        while entries.len() == PAGE_SIZE && current.pending.is_none() {
            if let Some(entry) = current.synthetic.pop_front() {
                current.pending = Some(entry);
                break;
            }
            let Some(item) = next_local_dir_entry(&mut current) else {
                break;
            };
            current.pending = convert(item, &mut current.seen_ids);
        }

        entries.sort_by(|a, b| {
            b.dir
                .cmp(&a.dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        let listing_id = current.id.clone();
        let listing_path = local_path_for_display(&current.path);
        let complete = current.pending.is_none();
        let next_cursor = if complete {
            None
        } else {
            let mut browser = browser.lock();
            browser
                .cursors
                .retain(|_, value| now.duration_since(value.touched) <= CURSOR_TTL);
            if browser.cursors.len() >= MAX_CURSORS {
                let oldest = browser
                    .cursors
                    .iter()
                    .min_by_key(|(_, value)| value.touched)
                    .map(|(token, _)| token.clone());
                if let Some(token) = oldest {
                    browser.cursors.remove(&token);
                }
            }
            browser.next_cursor = browser.next_cursor.wrapping_add(1);
            let token = format!("files-{:x}", browser.next_cursor);
            current.touched = Instant::now();
            browser.cursors.insert(token.clone(), current);
            Some(token)
        };
        Ok(LocalFileListing {
            id: listing_id,
            path: listing_path,
            platform: std::env::consts::OS.into(),
            entries,
            next_cursor,
            complete,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct LocalDirectoryChanged {
    token: u64,
    seq: u64,
    overflow: bool,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct LocalDirectoryWatchStarted {
    token: u64,
    lease_ms: u64,
}

#[tauri::command]
fn local_directory_watch(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    path: String,
) -> Result<LocalDirectoryWatchStarted, String> {
    const MAX_WATCHES: usize = 32;
    const DEBOUNCE: Duration = Duration::from_millis(100);
    const LEASE: Duration = Duration::from_secs(30 * 60);
    let directory = PathBuf::from(path)
        .canonicalize()
        .map_err(|error| error.to_string())?;
    if !directory.is_dir() {
        return Err("that watch target is not a directory".into());
    }
    if state.local_directory_watchers.lock().len() >= MAX_WATCHES {
        return Err("too many live local directory subscriptions".into());
    }
    let token = state
        .next_local_directory_watch
        .fetch_add(1, Ordering::Relaxed)
        .max(1);
    let (dirty_tx, dirty_rx) = mpsc::sync_channel::<()>(1);
    let overflow = Arc::new(AtomicBool::new(false));
    let callback_overflow = overflow.clone();
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
        match result {
            Ok(event) if matches!(event.kind, notify::EventKind::Access(_)) => return,
            Ok(_) => {}
            Err(_) => callback_overflow.store(true, Ordering::Relaxed),
        }
        let _ = dirty_tx.try_send(());
    })
    .map_err(|error| format!("couldn't create directory watcher: {error}"))?;
    watcher
        .watch(&directory, notify::RecursiveMode::NonRecursive)
        .map_err(|error| format!("couldn't watch that directory: {error}"))?;
    if let Some(public_desktop) = windows_desktop_watch_path(&directory) {
        watcher
            .watch(&public_desktop, notify::RecursiveMode::NonRecursive)
            .map_err(|error| format!("couldn't watch the public Desktop: {error}"))?;
    }
    let watchers = state.local_directory_watchers.clone();
    watchers
        .lock()
        .insert(token, LocalDirectoryWatch { _watcher: watcher });

    let expires_at = Instant::now() + LEASE;
    let worker_watchers = watchers.clone();
    let _ = std::thread::Builder::new()
        .name(format!("amst-local-files-watch-{token}"))
        .spawn(move || {
            let mut seq = 0_u64;
            loop {
                let now = Instant::now();
                if now >= expires_at {
                    worker_watchers.lock().remove(&token);
                    return;
                }
                match dirty_rx.recv_timeout(expires_at.saturating_duration_since(now)) {
                    Ok(()) => {}
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        worker_watchers.lock().remove(&token);
                        return;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
                std::thread::sleep(DEBOUNCE);
                if Instant::now() >= expires_at {
                    worker_watchers.lock().remove(&token);
                    return;
                }
                loop {
                    match dirty_rx.try_recv() {
                        Ok(()) => {}
                        Err(mpsc::TryRecvError::Empty) => break,
                        Err(mpsc::TryRecvError::Disconnected) => return,
                    }
                }
                seq = seq.saturating_add(1);
                if app
                    .emit(
                        "allmystuff://local-directory-changed",
                        LocalDirectoryChanged {
                            token,
                            seq,
                            overflow: overflow.swap(false, Ordering::Relaxed),
                        },
                    )
                    .is_err()
                {
                    worker_watchers.lock().remove(&token);
                    return;
                }
            }
        });
    Ok(LocalDirectoryWatchStarted {
        token,
        lease_ms: LEASE.as_millis() as u64,
    })
}

#[tauri::command]
fn local_directory_unwatch(state: State<'_, AppState>, token: u64) {
    state.local_directory_watchers.lock().remove(&token);
}

#[tauri::command]
async fn local_file_icon(path: String) -> Result<Option<String>, String> {
    #[cfg(windows)]
    {
        return tokio::task::spawn_blocking(move || {
            Ok(shell_icon::filesystem_icon(Path::new(&path)))
        })
        .await
        .map_err(|error| error.to_string())?;
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Ok(None)
    }
}

#[tauri::command]
async fn local_file_preview(path: String) -> Result<LocalPreview, String> {
    tokio::task::spawn_blocking(move || {
        use base64::Engine as _;
        const LIMIT: u64 = 4 * 1024 * 1024;
        let path = PathBuf::from(path);
        let meta = std::fs::metadata(&path).map_err(|e| e.to_string())?;
        if !meta.is_file() || meta.len() > LIMIT {
            return Ok(LocalPreview::Unsupported);
        }
        let ext = path
            .extension()
            .and_then(|x| x.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let mime = match ext.as_str() {
            "png" => Some("image/png"),
            "jpg" | "jpeg" => Some("image/jpeg"),
            "gif" => Some("image/gif"),
            "webp" => Some("image/webp"),
            "svg" => Some("image/svg+xml"),
            "bmp" => Some("image/bmp"),
            _ => None,
        };
        let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
        if let Some(mime) = mime {
            return Ok(LocalPreview::Image {
                mime: mime.into(),
                data: base64::engine::general_purpose::STANDARD.encode(bytes),
            });
        }
        const TEXT: &[&str] = &[
            "txt", "md", "rs", "ts", "js", "svelte", "json", "toml", "yaml", "yml", "css", "html",
            "xml", "sh", "ps1", "py", "go", "c", "h", "cpp", "hpp", "java", "log", "ini", "csv",
            "sql",
        ];
        if TEXT.contains(&ext.as_str()) {
            return Ok(LocalPreview::Text {
                text: String::from_utf8_lossy(&bytes).into_owned(),
            });
        }
        Ok(LocalPreview::Unsupported)
    })
    .await
    .map_err(|e| e.to_string())?
}

fn launch_native(path: &Path, reveal: bool) -> Result<(), String> {
    #[cfg(windows)]
    let status = {
        let mut command = std::process::Command::new("explorer.exe");
        if path == Path::new(RECYCLE_BIN_PARSE_NAME) {
            command.arg("shell:RecycleBinFolder");
        } else if reveal && path.is_file() {
            command.arg(format!("/select,{}", path.to_string_lossy()));
        } else {
            command.arg(path);
        }
        command.status()
    };
    #[cfg(target_os = "macos")]
    let status = {
        let mut command = std::process::Command::new("/usr/bin/open");
        if reveal {
            command.arg("-R");
        }
        command.arg(path).status()
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let status = {
        let target = if reveal && path.is_file() {
            path.parent().unwrap_or(path)
        } else {
            path
        };
        std::process::Command::new("xdg-open").arg(target).status()
    };
    status.map_err(|e| e.to_string()).and_then(|status| {
        if status.success() {
            Ok(())
        } else {
            Err(format!("native file browser exited with {status}"))
        }
    })
}

#[tauri::command]
async fn local_file_open(path: String, reveal: bool) -> Result<(), String> {
    tokio::task::spawn_blocking(move || launch_native(&PathBuf::from(path), reveal))
        .await
        .map_err(|e| e.to_string())?
}

#[cfg(windows)]
unsafe fn windows_shell_context_menu(
    hwnd: windows::Win32::Foundation::HWND,
    path: &Path,
) -> windows::core::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows::{
        core::{w, PCSTR, PCWSTR, PSTR},
        Win32::{
            Foundation::{LPARAM, POINT, WPARAM},
            System::Com::{CoInitializeEx, CoUninitialize, IBindCtx, COINIT_APARTMENTTHREADED},
            UI::{
                Input::KeyboardAndMouse::{
                    mouse_event, GetAsyncKeyState, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
                    VK_RBUTTON,
                },
                Shell::{
                    BHID_SFUIObject, IContextMenu, IShellItem, SHCreateItemFromParsingName,
                    SHObjectProperties, CMF_EXPLORE, CMF_NORMAL, CMINVOKECOMMANDINFO, GCS_VERBA,
                    SHOP_FILEPATH,
                },
                WindowsAndMessaging::{
                    CreatePopupMenu, CreateWindowExW, DestroyMenu, DestroyWindow, GetCursorPos,
                    MenuItemFromPoint, PostMessageW, SetForegroundWindow, TrackPopupMenu,
                    SW_SHOWNORMAL, TPM_RETURNCMD, TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WM_NULL,
                    WS_CHILD,
                },
            },
        },
    };

    let initialized = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
    let outcome = (|| -> windows::core::Result<()> {
        let shell_path = shell_icon::shell_compatible_path(path);
        let wide: Vec<u16> = shell_path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let item: IShellItem =
            unsafe { SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None::<&IBindCtx>)? };
        let shell_menu: IContextMenu =
            unsafe { item.BindToHandler(None::<&IBindCtx>, &BHID_SFUIObject)? };
        let menu_owner = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                w!(""),
                WS_CHILD,
                0,
                0,
                0,
                0,
                Some(hwnd),
                None,
                None,
                None,
            )?
        };
        let menu = unsafe { CreatePopupMenu()? };
        let mut replay_right_click = false;
        let menu_outcome = (|| -> windows::core::Result<()> {
            unsafe {
                shell_menu
                    .QueryContextMenu(menu, 0, 1, 0x7fff, CMF_NORMAL | CMF_EXPLORE)
                    .ok()?
            };

            let mut point = POINT::default();
            unsafe {
                GetCursorPos(&mut point)?;
                let _ = SetForegroundWindow(hwnd);
            }
            // Clear the initiating click's transition bit. If the popup later
            // dismisses on a different right-click, the fresh state below
            // tells us to hand that click back to the WebView.
            let _ = unsafe { GetAsyncKeyState(i32::from(VK_RBUTTON.0)) };
            let command = unsafe {
                TrackPopupMenu(
                    menu,
                    TPM_RETURNCMD | TPM_RIGHTBUTTON,
                    point.x,
                    point.y,
                    None,
                    menu_owner,
                    None,
                )
                .0 as u32
            };
            let mut dismissal_point = POINT::default();
            let outside_menu = unsafe { GetCursorPos(&mut dismissal_point) }.is_ok()
                && unsafe { MenuItemFromPoint(Some(menu_owner), menu, dismissal_point) } == -1;
            replay_right_click = command == 0 && outside_menu && {
                let state = unsafe { GetAsyncKeyState(i32::from(VK_RBUTTON.0)) } as u16;
                state & 0x8001 != 0
            };
            // Windows documents this nudge for repeated TrackPopupMenu calls.
            let _ = unsafe { PostMessageW(Some(menu_owner), WM_NULL, WPARAM(0), LPARAM(0)) };
            if command != 0 {
                let offset = command - 1;
                let mut verb_buffer = [0_u8; 64];
                let canonical_verb = unsafe {
                    shell_menu.GetCommandString(
                        offset as usize,
                        GCS_VERBA,
                        None,
                        PSTR(verb_buffer.as_mut_ptr()),
                        verb_buffer.len() as u32,
                    )
                }
                .ok()
                .and_then(|_| {
                    let end = verb_buffer.iter().position(|byte| *byte == 0)?;
                    std::str::from_utf8(&verb_buffer[..end]).ok()
                });
                // The generic numeric dispatch is correct for extension verbs,
                // but Windows can accept a .lnk Properties offset and then fail
                // to build its sheet outside Explorer. Use the Shell API whose
                // contract is specifically to invoke Properties on a file path.
                let properties_opened = canonical_verb
                    .is_some_and(|verb| verb.eq_ignore_ascii_case("properties"))
                    && shell_path.is_absolute()
                    && unsafe {
                        SHObjectProperties(
                            Some(hwnd),
                            SHOP_FILEPATH,
                            PCWSTR(wide.as_ptr()),
                            PCWSTR::null(),
                        )
                        .as_bool()
                    };
                if !properties_opened {
                    let invoke = CMINVOKECOMMANDINFO {
                        cbSize: std::mem::size_of::<CMINVOKECOMMANDINFO>() as u32,
                        hwnd,
                        // Shell command ids are passed as MAKEINTRESOURCEA offsets.
                        lpVerb: PCSTR(offset as usize as *const u8),
                        nShow: SW_SHOWNORMAL.0,
                        ..Default::default()
                    };
                    unsafe { shell_menu.InvokeCommand(&invoke)? };
                }
            }
            Ok(())
        })();
        let _ = unsafe { DestroyMenu(menu) };
        let _ = unsafe { DestroyWindow(menu_owner) };
        if replay_right_click {
            unsafe {
                mouse_event(MOUSEEVENTF_RIGHTDOWN, 0, 0, 0, 0);
                mouse_event(MOUSEEVENTF_RIGHTUP, 0, 0, 0, 0);
            }
        }
        menu_outcome
    })();
    if initialized {
        unsafe { CoUninitialize() };
    }
    outcome
}

#[tauri::command]
async fn local_file_context_menu(window: tauri::WebviewWindow, path: String) -> Result<(), String> {
    #[cfg(windows)]
    {
        #[allow(clippy::unnecessary_cast)]
        let hwnd = window.hwnd().map_err(|e| e.to_string())?.0 as usize;
        let (send, receive) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name("allmystuff-shell-menu".into())
            .spawn(move || {
                let hwnd = windows::Win32::Foundation::HWND(hwnd as *mut core::ffi::c_void);
                let result = unsafe { windows_shell_context_menu(hwnd, &PathBuf::from(path)) }
                    .map_err(|error| error.to_string());
                let _ = send.send(result);
            })
            .map_err(|error| error.to_string())?;
        let result = receive.await.map_err(|error| error.to_string())?;
        if let Err(error) = &result {
            tracing::warn!(%error, "couldn't show Windows Shell context menu");
        }
        return result;
    }
    #[cfg(not(windows))]
    Err("native file context menus are not implemented on this desktop yet".into())
}

fn safe_child(parent: &Path, name: &str) -> Result<PathBuf, String> {
    if name.trim().is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
        return Err("use a single file or folder name".into());
    }
    Ok(parent.join(name.trim()))
}

#[tauri::command]
async fn local_file_mkdir(
    parent: String,
    name: String,
    unique: Option<bool>,
) -> Result<LocalFileEntry, String> {
    tokio::task::spawn_blocking(move || {
        let parent = PathBuf::from(parent);
        let base_name = name.trim().to_string();
        safe_child(&parent, &base_name)?;
        let mut sequence = 1_u32;
        loop {
            let candidate_name = if sequence == 1 {
                base_name.clone()
            } else {
                format!("{base_name} ({sequence})")
            };
            let path = safe_child(&parent, &candidate_name)?;
            match std::fs::create_dir(&path) {
                Ok(()) => return local_file_entry(&path),
                Err(error)
                    if unique.unwrap_or(false)
                        && error.kind() == std::io::ErrorKind::AlreadyExists =>
                {
                    sequence = sequence
                        .checked_add(1)
                        .ok_or("couldn't find an available folder name")?;
                }
                Err(error) => return Err(error.to_string()),
            }
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn local_file_rename(path: String, name: String) -> Result<LocalFileEntry, String> {
    tokio::task::spawn_blocking(move || {
        let from = PathBuf::from(path);
        let parent = from.parent().ok_or("that item has no parent folder")?;
        let to = safe_child(parent, &name)?;
        if to.exists() {
            #[cfg(windows)]
            let case_only =
                from.to_string_lossy().to_lowercase() == to.to_string_lossy().to_lowercase();
            #[cfg(not(windows))]
            let case_only = false;
            if !case_only {
                return Err("an item with that name already exists".into());
            }
        }
        std::fs::rename(&from, &to).map_err(|e| e.to_string())?;
        local_file_entry(&to)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Delete uses the OS Trash/Recycle Bin. The canvas never offers an
/// irreversible unlink operation.
#[tauri::command]
async fn local_file_trash(paths: Vec<String>) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        for path in paths {
            trash::delete(PathBuf::from(path)).map_err(|e| e.to_string())?;
        }
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Forward one file request from a files window down its active files
/// route (the viewer side of a mesh-native file session).
#[tauri::command]
async fn file_send(
    state: State<'_, AppState>,
    route_id: String,
    event: serde_json::Value,
) -> Result<(), String> {
    state
        .node
        .request("file_send", json!({ "route_id": route_id, "event": event }))
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}
#[tauri::command]
async fn local_file_transfer_scan(
    state: State<'_, AppState>,
    id: String,
    paths: Vec<String>,
) -> Result<Value, String> {
    state
        .node
        .request("file_transfer_scan", json!({ "id": id, "paths": paths }))
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn local_file_transfer_start(
    state: State<'_, AppState>,
    id: String,
    route_id: String,
    paths: Vec<String>,
    destination: String,
    target_label: String,
    expected_files: u64,
    expected_folders: u64,
    expected_bytes: u64,
) -> Result<Value, String> {
    state
        .node
        .request(
            "file_transfer_start",
            json!({
                "id": id,
                "route_id": route_id,
                "paths": paths,
                "destination": destination,
                "target_label": target_label,
                "expected_files": expected_files,
                "expected_folders": expected_folders,
                "expected_bytes": expected_bytes,
            }),
        )
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn local_file_transfer_cancel(
    state: State<'_, AppState>,
    id: String,
) -> Result<bool, String> {
    let value = state
        .node
        .request("file_transfer_cancel", json!({ "id": id }))
        .await
        .map_err(|error| error.to_string())?;
    serde_json::from_value(value).map_err(|error| error.to_string())
}
#[tauri::command]
async fn local_file_transfer_operations(state: State<'_, AppState>) -> Result<Value, String> {
    state
        .node
        .request("file_transfer_operations", json!({}))
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn local_file_operation_dismiss(
    state: State<'_, AppState>,
    id: String,
) -> Result<bool, String> {
    let value = state
        .node
        .request("file_operation_dismiss", json!({ "id": id }))
        .await
        .map_err(|error| error.to_string())?;
    serde_json::from_value(value).map_err(|error| error.to_string())
}

#[tauri::command]
async fn fleetfiles_local_desktop(state: State<'_, AppState>) -> Result<Value, String> {
    state
        .node
        .request("fleetfiles_local_desktop", json!({}))
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn fleet_storage_status(state: State<'_, AppState>) -> Result<Value, String> {
    state
        .node
        .request("fleet_storage_status", json!({}))
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn fleet_storage_set_policy(
    state: State<'_, AppState>,
    policy: Value,
) -> Result<Value, String> {
    state
        .node
        .request("fleet_storage_set_policy", json!({ "policy": policy }))
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn fleet_storage_set_allocation(
    state: State<'_, AppState>,
    device: String,
    volume: String,
    quota_bytes: u64,
    enabled: bool,
) -> Result<Value, String> {
    state
        .node
        .request(
            "fleet_storage_set_allocation",
            json!({
                "device": device,
                "volume": volume,
                "quota_bytes": quota_bytes,
                "enabled": enabled,
            }),
        )
        .await
        .map_err(|error| error.to_string())
}
#[tauri::command]
async fn fleet_storage_set_device_role(
    state: State<'_, AppState>,
    device: String,
    role: String,
) -> Result<Value, String> {
    state
        .node
        .request(
            "fleet_storage_set_device_role",
            json!({
                "device": device,
                "role": role,
            }),
        )
        .await
        .map_err(|error| error.to_string())
}
#[tauri::command]
async fn fleet_service_profiles(state: State<'_, AppState>) -> Result<Value, String> {
    state
        .node
        .request("fleet_service_profiles", json!({}))
        .await
        .map_err(|error| error.to_string())
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct FleetStorageVolume {
    id: String,
    name: String,
    path: Option<String>,
    filesystem: Option<String>,
    total_bytes: u64,
    available_bytes: u64,
    removable: bool,
    kind: String,
}

#[tauri::command]
fn fleet_storage_local_volumes() -> Vec<FleetStorageVolume> {
    allmystuff_inventory::scan()
        .storage
        .into_iter()
        .map(|volume| FleetStorageVolume {
            id: volume.id,
            name: volume.name,
            path: volume.mount_point,
            filesystem: volume.filesystem,
            total_bytes: volume.total_bytes,
            available_bytes: volume.available_bytes,
            removable: volume.removable,
            kind: format!("{:?}", volume.kind).to_lowercase(),
        })
        .collect()
}

/// Register the calling files window's interest in a route's responses.
/// Frames buffer backend-side from route-activation; the window drains
/// them with `file_poll` on each `allmystuff://file-ready` poke. Same
/// pull-not-push shape as the terminal and video planes.
#[tauri::command]
async fn file_watch(app: tauri::AppHandle, route_id: String) -> u64 {
    let state = app.state::<AppState>();
    match state
        .node
        .request("file_watch", json!({ "route_id": route_id }))
        .await
    {
        Ok(v) => serde_json::from_value(v).unwrap_or_default(),
        Err(e) => {
            tracing::warn!("file_watch failed: {e:#}");
            0
        }
    }
}

/// Drain the queued responses for a files route as one raw batch:
/// `[u32 le len][frame json]…`, empty when nothing arrived.
#[tauri::command]
async fn file_poll(app: tauri::AppHandle, route_id: String) -> tauri::ipc::Response {
    let state = app.state::<AppState>();
    tauri::ipc::Response::new(
        state
            .node
            .request_bytes("file_poll", json!({ "route_id": route_id }))
            .await
            .unwrap_or_default(),
    )
}

/// Release a files window's claim on a route's responses (window closed).
/// Token-scoped and idempotent, like `term_unwatch`.
#[tauri::command]
async fn file_unwatch(app: tauri::AppHandle, route_id: String, token: u64) {
    let state = app.state::<AppState>();
    if let Err(e) = state
        .node
        .request(
            "file_unwatch",
            json!({ "route_id": route_id, "token": token }),
        )
        .await
    {
        tracing::warn!("file_unwatch failed: {e:#}");
    }
}

/// Route the coming `Read` request's chunks straight into this machine's
/// Downloads folder (instead of the window). Returns the destination path;
/// completion lands as `allmystuff://file-saved`. Call *before* sending
/// the request so the first chunk can't race the registration.
#[tauri::command]
async fn file_download(
    state: State<'_, AppState>,
    route_id: String,
    req: u64,
    name: String,
) -> Result<String, String> {
    let v = state
        .node
        .request(
            "file_download",
            json!({ "route_id": route_id, "req": req, "name": name }),
        )
        .await
        .map_err(|e| e.to_string())?;
    serde_json::from_value(v).map_err(|e| e.to_string())
}

/// Open (or focus) the dedicated files window for `node` — one OS window
/// per machine, the finder-like view of its disk. The window loads the
#[tauri::command]
async fn file_download_cancel(
    state: State<'_, AppState>,
    route_id: String,
    req: u64,
) -> Result<bool, String> {
    let value = state
        .node
        .request(
            "file_download_cancel",
            json!({ "route_id": route_id, "req": req }),
        )
        .await
        .map_err(|error| error.to_string())?;
    serde_json::from_value(value).map_err(|error| error.to_string())
}

/// same app with `?files=<node>`.
#[tauri::command]
async fn open_files_window(app: tauri::AppHandle, node: String) -> Result<(), String> {
    open_secondary_window(
        &app,
        &format!("files-{}", window_slug(&node)),
        format!("index.html?files={node}"),
        "AllMyStuff files",
        (940.0, 640.0),
        (480.0, 320.0),
        AUMID_FILES,
    )
}

#[tauri::command]
async fn open_files_workspace_window(
    app: tauri::AppHandle,
    target: String,
    title: String,
    instance: String,
) -> Result<(), String> {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(target);
    let label = format!("files-workspace-{}", window_slug(&instance));
    open_secondary_window(
        &app,
        &label,
        format!("index.html?files-workspace={encoded}"),
        if title.trim().is_empty() {
            "AllMyStuff Files"
        } else {
            &title
        },
        (1120.0, 760.0),
        (640.0, 420.0),
        AUMID_FILES,
    )
}

// ---- sites (the reverse proxy) -----------------------------------------

/// This machine's discovered listening TCP services (with an active banner
/// probe), so the Sites tab can offer each to expose. The probe does
/// blocking socket I/O, so it runs off the command executor.
#[tauri::command]
async fn site_scan(
    state: State<'_, AppState>,
) -> Result<Vec<allmystuff_inventory::ListeningService>, String> {
    let v = state
        .node
        .request("site_scan", json!({}))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::from_value(v).map_err(|e| e.to_string())
}

/// The services this machine currently advertises, as id → display name
/// (empty name = the classified default).
#[tauri::command]
async fn site_exposed(app: tauri::AppHandle) -> std::collections::BTreeMap<String, String> {
    let state = app.state::<AppState>();
    match state.node.request("site_exposed", json!({})).await {
        Ok(v) => serde_json::from_value(v).unwrap_or_default(),
        Err(e) => {
            tracing::warn!("site_exposed failed: {e:#}");
            Default::default()
        }
    }
}

/// Set which listening services this machine advertises (id → display name).
/// Re-broadcasts presence so peers' Sites tabs update; returns the new set.
#[tauri::command]
async fn site_set_exposed(
    state: State<'_, AppState>,
    exposed: std::collections::BTreeMap<String, String>,
) -> Result<std::collections::BTreeMap<String, String>, String> {
    let v = state
        .node
        .request("site_set_exposed", json!({ "exposed": exposed }))
        .await
        .map_err(|e| e.to_string())?;
    serde_json::from_value(v).map_err(|e| e.to_string())
}

/// Map a peer's site to a local port — set up the reverse-proxy route and
/// bind a local listener. Returns `{ localPort }`.
#[tauri::command]
async fn site_map(state: State<'_, AppState>, node: String, port: u16) -> Result<Value, String> {
    state
        .node
        .request("site_map", json!({ "node": node, "port": port }))
        .await
        .map_err(|e| e.to_string())
}

/// Tear a site mapping down (unbind the local listener, drop the route).
#[tauri::command]
async fn site_unmap(state: State<'_, AppState>, node: String, port: u16) -> Result<(), String> {
    state
        .node
        .request("site_unmap", json!({ "node": node, "port": port }))
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Every site this machine currently has mapped: `{ node, port, localPort }`.
#[tauri::command]
async fn site_mappings(app: tauri::AppHandle) -> Vec<Value> {
    let state = app.state::<AppState>();
    match state.node.request("site_mappings", json!({})).await {
        Ok(v) => serde_json::from_value(v).unwrap_or_default(),
        Err(e) => {
            tracing::warn!("site_mappings failed: {e:#}");
            Vec::new()
        }
    }
}

/// Ask a co-owned fleet machine for its full site list, to manage its
/// exposure from its drawer. The reply arrives as `allmystuff://node-sites`.
#[tauri::command]
async fn site_remote_list(state: State<'_, AppState>, node: String) -> Result<(), String> {
    state
        .node
        .request("site_remote_list", json!({ "node": node }))
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Tell a co-owned fleet machine to advertise exactly `exposed` (id → name).
#[tauri::command]
async fn site_remote_set_exposed(
    state: State<'_, AppState>,
    node: String,
    exposed: std::collections::BTreeMap<String, String>,
) -> Result<(), String> {
    state
        .node
        .request(
            "site_remote_set_exposed",
            json!({ "node": node, "exposed": exposed }),
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Open (or focus) a dedicated console window for `node` — its own OS
/// window, so several remote consoles can be on screen at once. The window
/// loads the same app with `?console=<node>`, which renders just the
/// console for that machine.
#[tauri::command]
async fn open_console_window(app: tauri::AppHandle, node: String) -> Result<(), String> {
    open_secondary_window(
        &app,
        &format!("console-{}", window_slug(&node)),
        format!("index.html?console={node}"),
        "AllMyStuff console",
        (1100.0, 740.0),
        (560.0, 380.0),
        AUMID_CONSOLE,
    )
}

/// Open (or focus) the dedicated window for one virtual room — the call
/// itself, in its own OS window like the console / terminal / files
/// sessions, so it can be moved, resized and full-screened. The window
/// loads the same app with `?room=<room id>`.
#[tauri::command]
async fn open_room_window(app: tauri::AppHandle, room: String) -> Result<(), String> {
    open_secondary_window(
        &app,
        &format!("room-{}", window_slug(&room)),
        format!("index.html?room={room}"),
        "AllMyStuff room",
        (1180.0, 760.0),
        (640.0, 440.0),
        AUMID_ROOM,
    )
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
    decoder: Option<String>,
) -> Result<(), String> {
    let decoder = match decoder.as_deref() {
        None | Some("automatic") => "automatic",
        Some("software") => "software",
        Some(other) => {
            return Err(format!(
                "unsupported local decoder preference {other:?} (automatic | software)"
            ))
        }
    };
    open_secondary_window(
        &app,
        &format!("video-{}", window_slug(&key)),
        // The key carries capability/route ids (colons, the route arrow) —
        // percent-encode so the query survives; URLSearchParams decodes.
        format!("index.html?video={}&decoder={decoder}", query_encode(&key)),
        &title,
        (880.0, 560.0),
        (380.0, 260.0),
        AUMID_VIDEO,
    )
}

/// Pop the CEC Support console out into its own window (`?cec=1`). A single
/// fixed label so re-opening focuses the existing console instead of stacking
/// a second one. The technician's whole help-desk surface, off on its own
/// screen while the main window keeps the device graph.
#[tauri::command]
async fn open_cec_window(app: tauri::AppHandle) -> Result<(), String> {
    open_secondary_window(
        &app,
        "cec-console",
        "index.html?cec=1".to_string(),
        "CEC Console",
        (960.0, 720.0),
        (420.0, 480.0),
        AUMID_CEC,
    )
}

/// Open (or focus) the pop-out chat window for one CEC customer (`?chat=<peer>`)
/// — the sibling of `open_console_window`, so a technician can message a
/// customer beside the live session. One window per customer (`chat-<peer>`),
/// so re-opening focuses the existing thread instead of stacking a second one.
/// Built through the same `open_secondary_window` helper as every other
/// secondary window, so it gets its taskbar identity like the rest.
#[tauri::command]
async fn open_chat_window(app: tauri::AppHandle, peer: String) -> Result<(), String> {
    open_secondary_window(
        &app,
        &format!("chat-{}", window_slug(&peer)),
        // The peer is a node id (may carry non-label chars) — percent-encode
        // so the query survives; the frontend's URLSearchParams decodes it.
        format!("index.html?chat={}", query_encode(&peer)),
        "AllMyStuff chat",
        (420.0, 620.0),
        (320.0, 420.0),
        AUMID_CHAT,
    )
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

// AppUserModelIDs for the secondary windows, so each *kind* groups under its own
// taskbar button rather than stacking under the main app — terminals together,
// files together, and so on, each separately pinnable. The main window keeps the
// process default. The strings are stable identities (Windows keys pins and
// grouping off them); they are not the bundle identifier on purpose. Referenced
// on every platform (the call sites pass them), used only on Windows.
const AUMID_TERMINAL: &str = "works.allmystuff.terminal";
const AUMID_CONSOLE: &str = "works.allmystuff.console";
const AUMID_FILES: &str = "works.allmystuff.files";
const AUMID_ROOM: &str = "works.allmystuff.room";
const AUMID_VIDEO: &str = "works.allmystuff.video";
const AUMID_CEC: &str = "works.allmystuff.cec";
const AUMID_CHAT: &str = "works.allmystuff.chat";

/// Give a secondary window its own taskbar identity (an explicit per-window
/// AppUserModelID) so it groups separately from the main AllMyStuff app and is
/// separately pinnable. Windows only — a no-op everywhere else. Best-effort: a
/// failure just leaves the window on the default grouping, never an error.
///
/// Per-window (not per-process) is the point: every Tauri window lives in one
/// process, so `SetCurrentProcessExplicitAppUserModelID` can't separate them —
/// only the window's shell property store (`PKEY_AppUserModel_ID`) can.
///
/// The shell-store write is marshalled to the **main (event-loop) thread**.
/// It calls `SHGetPropertyStoreForWindow`, a shell/COM API, and the window
/// builder runs this from an *async* command — i.e. a runtime worker thread
/// with no COM initialized. Touching the shell store there is undefined, and
/// with `panic = abort` a fault takes the whole GUI down (that was the crash
/// when opening a terminal window on Windows). The main thread is the one tao
/// initialized COM on (`OleInitialize`) and the one the window belongs to, so
/// the write happens there.
#[cfg_attr(not(windows), allow(unused_variables))]
fn set_taskbar_identity(app: &tauri::AppHandle, label: &str, aumid: &'static str) {
    #[cfg(windows)]
    {
        let label = label.to_string();
        let app_for_lookup = app.clone();
        let _ = app.run_on_main_thread(move || {
            if let Some(window) = app_for_lookup.get_webview_window(&label) {
                apply_taskbar_identity(&window, aumid);
            }
        });
    }
}

/// The Windows shell-store write behind [`set_taskbar_identity`]. MUST run on a
/// COM-initialized thread that owns the window — the main thread (see the
/// caller). Best-effort: every failure path is a logged no-op.
#[cfg(windows)]
fn apply_taskbar_identity(window: &tauri::WebviewWindow, aumid: &str) {
    use windows::core::{GUID, PWSTR};
    use windows::Win32::Foundation::{HWND, PROPERTYKEY};
    use windows::Win32::System::Com::StructuredStorage::{
        PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
    };
    use windows::Win32::System::Variant::VT_LPWSTR;
    use windows::Win32::UI::Shell::PropertiesSystem::{
        IPropertyStore, SHGetPropertyStoreForWindow,
    };

    // PKEY_AppUserModel_ID = {9F4C2855-9F79-4B39-A8D0-E1D42DE1D5F3}, 5.
    const PKEY_APPUSERMODEL_ID: PROPERTYKEY = PROPERTYKEY {
        fmtid: GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3),
        pid: 5,
    };

    // Tauri links an older `windows` crate than this GUI, so its `HWND` is a
    // different type — bridge through the raw pointer. The `as *mut c_void`
    // is a no-op on the currently pinned crate pair (clippy would flag it),
    // but it's kept on purpose: if Tauri's `HWND` ever reverts to an `isize`
    // representation the cast is what keeps this compiling.
    #[allow(clippy::unnecessary_cast)]
    let raw = match window.hwnd() {
        Ok(h) => h.0 as *mut core::ffi::c_void,
        Err(e) => {
            tracing::warn!("taskbar identity: no window handle ({e})");
            return;
        }
    };

    // A null-terminated wide copy of the id; it must outlive `SetValue`,
    // which copies the string into the store (see the `drop` at the end).
    let mut wide: Vec<u16> = aumid.encode_utf16().chain(std::iter::once(0)).collect();
    // A VT_LPWSTR PROPVARIANT pointing at `wide` (windows 0.61 has no
    // single-string PROPVARIANT constructor, so build the union by hand).
    //
    // The whole value is wrapped in `ManuallyDrop` for memory safety, NOT
    // ergonomics: windows-rs gives `PROPVARIANT` a `Drop` that calls
    // `PropVariantClear`, which for a VT_LPWSTR would `CoTaskMemFree(pwszVal)`.
    // But `pwszVal` is our `Vec`, never COM-allocated — freeing it on the COM
    // heap corrupts the heap (`STATUS_HEAP_CORRUPTION`), and `drop(wide)` would
    // then double-free it. (The *inner* `ManuallyDrop` is just the union
    // field's required type and does NOT suppress `PROPVARIANT`'s own `Drop`,
    // which is the trap the first version fell into.) `SetValue` copies the
    // string into the store, so nothing here owns COM memory to leak.
    let value = core::mem::ManuallyDrop::new(PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: core::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_LPWSTR,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 {
                    pwszVal: PWSTR(wide.as_mut_ptr()),
                },
            }),
        },
    });

    unsafe {
        let store: IPropertyStore = match SHGetPropertyStoreForWindow(HWND(raw)) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("taskbar identity: property store unavailable ({e})");
                return;
            }
        };
        if store.SetValue(&PKEY_APPUSERMODEL_ID, &*value).is_ok() {
            let _ = store.Commit();
        }
    }
    // Keep `wide` alive past `SetValue` (its raw pointer rode inside
    // `value`); a raw pointer creates no borrow, so without this the buffer
    // could be freed before the store reads it.
    drop(wide);
}

/// Current peers + live route states.
#[tauri::command]
async fn session_snapshot(app: tauri::AppHandle) -> Value {
    let state = app.state::<AppState>();
    match state.node.request("session_snapshot", json!({})).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("session_snapshot failed: {e:#}");
            Value::Null
        }
    }
}

/// The owned-fleet roster: the shared key and the devices this owner has
/// claimed (and that have converged via gossip). Drives the Fleet settings
/// view; updated live by the `allmystuff://owned` event.
#[tauri::command]
async fn owned_roster(app: tauri::AppHandle) -> Value {
    let state = app.state::<AppState>();
    match state.node.request("owned_roster", json!({})).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("owned_roster failed: {e:#}");
            Value::Null
        }
    }
}

/// Leave the fleet this device belongs to (and release its owner) — the
/// remaining members converge on the bumped roster without us.
#[tauri::command]
async fn fleet_leave(state: State<'_, AppState>) -> Result<(), String> {
    state
        .node
        .request("fleet_leave", json!({}))
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Danger Zone: leave the fleet and forget every network the daemon holds
/// (keeps this device's identity). The daemon exits afterward; the caller
/// follows with `restart_app` so the whole stack reloads clean.
#[tauri::command]
async fn reset_networking(state: State<'_, AppState>) -> Result<(), String> {
    state
        .node
        .request("reset_networking", json!({}))
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Danger Zone: factory reset — wipe this device's entire state (identity,
/// config, all networks, fleet ownership) so it's brand-new to every peer. The
/// node clears its ownership and the daemon wipes `~/.myownmesh` and exits; the
/// caller follows with `restart_app`.
#[tauri::command]
async fn factory_reset(state: State<'_, AppState>) -> Result<(), String> {
    state
        .node
        .request("factory_reset", json!({}))
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Evict a device from the fleet (owner-only; the daemon enforces it). `code`
/// is the owner's custody second factor when fleet MFA is enrolled.
#[tauri::command]
async fn fleet_kick(
    state: State<'_, AppState>,
    device: String,
    code: Option<String>,
) -> Result<(), String> {
    state
        .node
        .request("fleet_kick", json!({ "device": device, "code": code }))
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Name (or rename) the fleet this device belongs to. Members only; the
/// renamed roster gossips out and converges like any membership change.
#[tauri::command]
async fn fleet_set_name(state: State<'_, AppState>, name: String) -> Result<(), String> {
    state
        .node
        .request("fleet_set_name", json!({ "name": name }))
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Grant a fleet member a role: "manager" (controller) or "owner". Owner-only;
/// the daemon enforces the closed network's quorum.
#[tauri::command]
async fn fleet_grant_role(
    state: State<'_, AppState>,
    device: String,
    role: String,
    code: Option<String>,
) -> Result<(), String> {
    state
        .node
        .request(
            "fleet_grant_role",
            json!({ "device": device, "role": role, "code": code }),
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Withdraw a fleet member's role, back to a plain member. Owner-only. `code`
/// is the custody second factor when fleet MFA is enrolled.
#[tauri::command]
async fn fleet_revoke_role(
    state: State<'_, AppState>,
    device: String,
    code: Option<String>,
) -> Result<(), String> {
    state
        .node
        .request(
            "fleet_revoke_role",
            json!({ "device": device, "code": code }),
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Designate the fleet's infra hubs — the owner-signed network-wide shape
/// every member's daemon converges onto (daemon ≥ 0.2.36). Pass the full hub
/// set each call; an empty set returns the fleet to full mesh. Owner-only.
/// `code` is the custody second factor when fleet MFA is enrolled.
#[tauri::command]
async fn fleet_set_hubs(
    state: State<'_, AppState>,
    hubs: Vec<String>,
    redundancy: Option<u32>,
    code: Option<String>,
) -> Result<(), String> {
    state
        .node
        .request(
            "fleet_set_hubs",
            json!({ "hubs": hubs, "redundancy": redundancy, "code": code }),
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Whether this device has enrolled a custody authenticator for the fleet's
/// closed network: `{ "enrolled": bool, "no_fleet"?: true }`.
#[tauri::command]
async fn fleet_mfa_status(state: State<'_, AppState>) -> Result<Value, String> {
    state
        .node
        .request("fleet_mfa_status", json!({}))
        .await
        .map_err(|e| e.to_string())
}

/// Enroll a custody authenticator for the fleet. Returns the secret,
/// `otpauth://` URI, and one-time recovery codes (shown once).
#[tauri::command]
async fn fleet_mfa_enroll(state: State<'_, AppState>) -> Result<Value, String> {
    state
        .node
        .request("fleet_mfa_enroll", json!({}))
        .await
        .map_err(|e| e.to_string())
}

/// Remove the fleet's custody authenticator (requires a valid code).
#[tauri::command]
async fn fleet_mfa_disable(state: State<'_, AppState>, code: String) -> Result<Value, String> {
    state
        .node
        .request("fleet_mfa_disable", json!({ "code": code }))
        .await
        .map_err(|e| e.to_string())
}

// ---- CEC Support -------------------------------------------------------
//
// Thin passthroughs to the node's `cec_*` control commands (the verbatim
// surface the CEC Support client app and the CEC settings tab both use). The
// `cec://*` events reach the frontend through the existing event pump, which
// forwards every `UiSink::emit` by name.

/// This node's CEC snapshot: its support number, Silent room, role, hosting.
#[tauri::command]
async fn cec_status(state: State<'_, AppState>) -> Result<Value, String> {
    state
        .node
        .request("cec_status", json!({}))
        .await
        .map_err(|e| e.to_string())
}

/// Technician: dial a customer by number, joining their secret Silent mesh and
/// connecting to the one peer there (which then shows as an ordinary graph
/// peer). Returns `{ node }`.
#[tauri::command]
async fn cec_dial(
    state: State<'_, AppState>,
    number: String,
    agent_name: Option<String>,
) -> Result<Value, String> {
    state
        .node
        .request(
            "cec_dial",
            json!({ "number": number, "agent_name": agent_name }),
        )
        .await
        .map_err(|e| e.to_string())
}

/// Technician: dial a specific customer by node id — the raised-hand answer
/// (the queue hands us a node, not a number). This bridge was missing, so
/// every "answer" `invoke("cec_dial_node")` failed with "Command not found"
/// even though the node has handled it all along. Returns `{ node }`.
#[tauri::command]
async fn cec_dial_node(
    state: State<'_, AppState>,
    node: String,
    agent_name: Option<String>,
) -> Result<Value, String> {
    state
        .node
        .request(
            "cec_dial_node",
            json!({ "node": node, "agent_name": agent_name }),
        )
        .await
        .map_err(|e| e.to_string())
}

/// Technician: the dialed-customer directory — every machine *attempted*
/// (nodeless until discovery succeeds), with live reachability. Drives the CEC
/// tab's Client meshes list; without this command the list can never load.
#[tauri::command]
async fn cec_dialed(state: State<'_, AppState>) -> Result<Value, String> {
    state
        .node
        .request("cec_dialed", json!({}))
        .await
        .map_err(|e| e.to_string())
}

/// Technician: the customers currently asking for help on the global help
/// room, longest-waiting first. Read-only — joining the room is
/// `cec_help_watch`'s job, an explicit opt-in.
#[tauri::command]
async fn cec_help_list(state: State<'_, AppState>) -> Result<Value, String> {
    state
        .node
        .request("cec_help_list", json!({}))
        .await
        .map_err(|e| e.to_string())
}

/// Technician: join or leave the global help room — the "Watch the help
/// queue" toggle. The daemon persists the membership, so the choice survives
/// restarts. This command going missing is why the toggle once did nothing:
/// the frontend invoked it, Tauri rejected the unknown command, and the
/// permissive tryInvoke wrapper swallowed the evidence.
#[tauri::command]
async fn cec_help_watch(state: State<'_, AppState>, on: bool) -> Result<Value, String> {
    state
        .node
        .request("cec_help_watch", json!({ "on": on }))
        .await
        .map_err(|e| e.to_string())
}

/// Technician: stop whatever the in-flight dial is trying (discovery poll +
/// connect-request re-sends). The attempt row stays in the directory.
#[tauri::command]
async fn cec_cancel_dial(state: State<'_, AppState>) -> Result<Value, String> {
    state
        .node
        .request("cec_cancel_dial", json!({}))
        .await
        .map_err(|e| e.to_string())
}

/// The inbound technician connect-requests awaiting a choice.
#[tauri::command]
async fn cec_pending(state: State<'_, AppState>) -> Result<Value, String> {
    state
        .node
        .request("cec_pending", json!({}))
        .await
        .map_err(|e| e.to_string())
}

/// Customer: approve a technician at a scope (once / three_hours / forever).
#[tauri::command]
async fn cec_approve(
    state: State<'_, AppState>,
    tech: String,
    scope: String,
    session_id: String,
    want_control: bool,
) -> Result<Value, String> {
    state
        .node
        .request(
            "cec_approve",
            json!({
                "tech": tech,
                "scope": scope,
                "session_id": session_id,
                "want_control": want_control,
            }),
        )
        .await
        .map_err(|e| e.to_string())
}

/// Customer: decline a pending connect-request.
#[tauri::command]
async fn cec_deny(
    state: State<'_, AppState>,
    tech: String,
    session_id: String,
) -> Result<Value, String> {
    state
        .node
        .request(
            "cec_deny",
            json!({ "tech": tech, "session_id": session_id }),
        )
        .await
        .map_err(|e| e.to_string())
}

/// Customer: "Forget this technician" — revoke every grant and tear down.
#[tauri::command]
async fn cec_revoke(state: State<'_, AppState>, tech: String) -> Result<Value, String> {
    state
        .node
        .request("cec_revoke", json!({ "tech": tech }))
        .await
        .map_err(|e| e.to_string())
}

/// Customer: the live consent grants.
#[tauri::command]
async fn cec_grants(state: State<'_, AppState>) -> Result<Value, String> {
    state
        .node
        .request("cec_grants", json!({}))
        .await
        .map_err(|e| e.to_string())
}

/// Technician: send a chat line to a dialed customer. The node persists it,
/// relays it over the session, and echoes it back on `cec://chat`; returns
/// the assigned `{ id, ts }`. Unregistered until now, every send from the
/// chat window died as a swallowed "command not found" — the optimistic
/// bubble showed and nothing left the machine (the cec_dial_node /
/// cec_help_watch ghost all over again).
#[tauri::command]
async fn cec_chat_send(
    state: State<'_, AppState>,
    peer: String,
    text: String,
) -> Result<Value, String> {
    state
        .node
        .request("cec_chat_send", json!({ "peer": peer, "text": text }))
        .await
        .map_err(|e| e.to_string())
}

/// Technician: the stored chat thread with one customer, oldest-first —
/// what fills the window on open, so a thread survives closing it.
#[tauri::command]
async fn cec_chat_history(state: State<'_, AppState>, peer: String) -> Result<Value, String> {
    state
        .node
        .request("cec_chat_history", json!({ "peer": peer }))
        .await
        .map_err(|e| e.to_string())
}

/// The per-node gear "Forget this node": drop it from the graph + roster, tear
/// its session down, and end any CEC session.
#[tauri::command]
async fn forget_node(state: State<'_, AppState>, node: String) -> Result<Value, String> {
    state
        .node
        .request("forget_node", json!({ "node": node }))
        .await
        .map_err(|e| e.to_string())
}

/// Fan one room-plane message (invite / join / leave / chat) out to the
/// given members. Best-effort per member; returns how many the daemon
/// actually dispatched to, so the UI can say when a line reached nobody.
#[tauri::command]
async fn room_send(
    state: State<'_, AppState>,
    members: Vec<String>,
    message: serde_json::Value,
) -> Result<u32, String> {
    let v = state
        .node
        .request(
            "room_send",
            json!({ "members": members, "message": message }),
        )
        .await
        .map_err(|e| e.to_string())?;
    serde_json::from_value(v).map_err(|e| e.to_string())
}

// ---- Shared Files (the call's shared-download area) ---------------------

/// Offer files into a room's Shared Files area — register each path with
/// the members allowed to fetch it, returning the `{ token, name, size }`
/// the GUI hands to the room's host for its shared list. The bytes never
/// leave this machine until a member fetches them by token.
#[tauri::command]
async fn room_share_files(
    app: tauri::AppHandle,
    members: Vec<String>,
    paths: Vec<String>,
) -> Vec<allmystuff_protocol::SharedFileMeta> {
    let state = app.state::<AppState>();
    match state
        .node
        .request(
            "room_share_files",
            json!({ "members": members, "paths": paths }),
        )
        .await
    {
        Ok(v) => serde_json::from_value(v).unwrap_or_default(),
        Err(e) => {
            tracing::warn!("room_share_files failed: {e:#}");
            Vec::new()
        }
    }
}

/// Refresh the members allowed to fetch a set of shared tokens (the room's
/// roster changed while the files were on offer).
#[tauri::command]
async fn room_set_share_peers(app: tauri::AppHandle, tokens: Vec<String>, members: Vec<String>) {
    let state = app.state::<AppState>();
    if let Err(e) = state
        .node
        .request(
            "room_set_share_peers",
            json!({ "tokens": tokens, "members": members }),
        )
        .await
    {
        tracing::warn!("room_set_share_peers failed: {e:#}");
    }
}

/// Stop offering a set of shared files (the uploader removed them or left).
#[tauri::command]
async fn room_unshare(app: tauri::AppHandle, tokens: Vec<String>) {
    let state = app.state::<AppState>();
    if let Err(e) = state
        .node
        .request("room_unshare", json!({ "tokens": tokens }))
        .await
    {
        tracing::warn!("room_unshare failed: {e:#}");
    }
}

// ---- mesh control passthroughs ----------------------------------------

#[tauri::command]
async fn mesh_status(state: State<'_, AppState>) -> Result<Value, String> {
    state
        .node
        .request("mesh_status", json!({}))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn mesh_identity(state: State<'_, AppState>) -> Result<Value, String> {
    state
        .node
        .request("mesh_identity", json!({}))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn mesh_networks(state: State<'_, AppState>) -> Result<Value, String> {
    state
        .node
        .request("mesh_networks", json!({}))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn mesh_peers(state: State<'_, AppState>, network: String) -> Result<Value, String> {
    state
        .node
        .request("mesh_peers", json!({ "network": network }))
        .await
        .map_err(|e| e.to_string())
}

/// The engine's daemon-link status as last emitted on
/// `allmystuff://subscription` — the poll-safe truth for a front-end that
/// subscribed after the one-shot event fired. Distinguishes "node socket
/// answers" from "the mesh behind it is live". (`mesh_status` above is the
/// raw daemon Status passthrough — a different question.)
#[tauri::command]
async fn link_status(state: State<'_, AppState>) -> Result<Value, String> {
    state
        .node
        .request("link_status", json!({}))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn mesh_network_add(state: State<'_, AppState>, config: Value) -> Result<Value, String> {
    state
        .node
        .request("mesh_network_add", json!({ "config": config }))
        .await
        .map_err(|e| e.to_string())
}

/// The whole daemon config — every network with its full signaling / STUN /
/// TURN settings. The Servers settings pane reads this to populate its editor
/// (`NetworksList` only carries summaries).
#[tauri::command]
async fn mesh_config_show(state: State<'_, AppState>) -> Result<Value, String> {
    state
        .node
        .request("mesh_config_show", json!({}))
        .await
        .map_err(|e| e.to_string())
}

/// Replace one network's config (its signaling / STUN / TURN servers, label,
/// etc.). The daemon hot-applies cosmetic changes and restarts the transport
/// for server changes; the node re-subscribes afterwards so the session
/// reconnects.
#[tauri::command]
async fn mesh_network_update(state: State<'_, AppState>, config: Value) -> Result<Value, String> {
    state
        .node
        .request("mesh_network_update", json!({ "config": config }))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn mesh_roster_approve(
    state: State<'_, AppState>,
    network: String,
    device_id: String,
    label: Option<String>,
) -> Result<Value, String> {
    state
        .node
        .request(
            "mesh_roster_approve",
            json!({ "network": network, "device_id": device_id, "label": label }),
        )
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn mesh_roster_remove(
    state: State<'_, AppState>,
    network: String,
    device_id: String,
) -> Result<Value, String> {
    state
        .node
        .request(
            "mesh_roster_remove",
            json!({ "network": network, "device_id": device_id }),
        )
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn mesh_roster_list(state: State<'_, AppState>, network: String) -> Result<Value, String> {
    state
        .node
        .request("mesh_roster_list", json!({ "network": network }))
        .await
        .map_err(|e| e.to_string())
}

/// Ask the daemon for a fresh, valid network id (the shareable handle peers
/// join with). Used by the "create network" flow.
#[tauri::command]
async fn mesh_network_id_generate(state: State<'_, AppState>) -> Result<Value, String> {
    state
        .node
        .request("mesh_network_id_generate", json!({}))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn mesh_network_remove(state: State<'_, AppState>, network: String) -> Result<Value, String> {
    state
        .node
        .request("mesh_network_remove", json!({ "network": network }))
        .await
        .map_err(|e| e.to_string())
}

/// The networks currently switched off (their full parked configs), for
/// the pill menu's disabled rows.
#[tauri::command]
async fn disabled_networks(app: tauri::AppHandle) -> Vec<Value> {
    let state = app.state::<AppState>();
    match state.node.request("disabled_networks", json!({})).await {
        Ok(v) => serde_json::from_value(v).unwrap_or_default(),
        Err(e) => {
            tracing::warn!("disabled_networks failed: {e:#}");
            Vec::new()
        }
    }
}

/// Switch a network off or back on without deleting it. Off = leave the
/// daemon (peers drop, nothing is advertised there any more) but park the
/// full config locally; on = hand the parked config back to the daemon.
/// The network's roster file survives on disk either way, so approvals
/// aren't lost in between. `network` may be the config id or network id.
#[tauri::command]
async fn network_set_enabled(
    state: State<'_, AppState>,
    network: String,
    enabled: bool,
) -> Result<Value, String> {
    state
        .node
        .request(
            "network_set_enabled",
            json!({ "network": network, "enabled": enabled }),
        )
        .await
        .map_err(|e| e.to_string())
}

/// Reconnect a joined network *in place* — redial signaling and renegotiate
/// ICE without leaving the room. The non-destructive twin of a leave+rejoin:
/// peers keep their sessions and app-level state, so this is what the refresh
/// controls drive instead of `network_set_enabled(false)`+`(true)`. `peer`
/// omitted reconnects every peer on the network; `peer` set reconnects just
/// that one node (the per-node refresh). `network` may be the config id or
/// network id.
#[tauri::command]
async fn network_reconnect(
    state: State<'_, AppState>,
    network: Option<String>,
    peer: Option<String>,
) -> Result<Value, String> {
    state
        .node
        .request(
            "mesh_network_reconnect",
            json!({ "network": network, "peer": peer }),
        )
        .await
        .map_err(|e| e.to_string())
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
    label: String,
) -> Result<Value, String> {
    state
        .node
        .request("mesh_identity_set_label", json!({ "label": label }))
        .await
        .map_err(|e| e.to_string())
}

// ---- self-update (AllMyStuff's own updater, not the daemon's) ----------

#[tauri::command]
async fn update_status() -> Result<Value, String> {
    serde_json::to_value(allmystuff_updater::status().map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())
}

/// Every independently replaceable part of the local install. Runtime RPCs
/// are used for the two long-lived processes so this reports what is actually
/// executing, while updater markers cover optional sibling tools on disk.
#[tauri::command]
async fn component_status(state: State<'_, AppState>) -> Result<Value, String> {
    let app_pin = env!("CARGO_PKG_VERSION");
    let node = state.node.request("node_version", json!({})).await.ok();
    let mesh = state.node.request("mesh_status", json!({})).await.ok();
    let service = tokio::task::spawn_blocking(|| {
        allmystuff_service::status_value(false).unwrap_or_else(|_| json!({}))
    })
    .await
    .map_err(|e| format!("service status task failed: {e}"))?;
    let artifacts = allmystuff_updater::installed_artifact_statuses();

    let node_version = node
        .as_ref()
        .and_then(|v| v.get("version"))
        .and_then(Value::as_str);
    let mesh_version = mesh
        .as_ref()
        .and_then(|v| v.get("version"))
        .and_then(Value::as_str);
    let mesh_disk = allmystuff_node::daemon_spawn::installed_daemon_version()
        .await
        .ok()
        .flatten();
    let artifact = |name: &str| {
        artifacts
            .iter()
            .find(|a| a.component == name && a.installed)
            .and_then(|a| a.version.clone())
    };

    let mut rows = vec![
        json!({ "id": "gui", "label": "AllMyStuff GUI", "current": app_pin, "pinned": app_pin, "detail": "Running desktop app" }),
        json!({ "id": "serve", "label": "AllMyStuff Serve", "current": node_version, "pinned": app_pin, "detail": "Running mesh and media backend" }),
        json!({ "id": "myownmesh", "label": "MyOwnMesh Serve", "current": mesh_version, "pinned": allmystuff_node::daemon_spawn::daemon_pin(), "installed": mesh_disk, "detail": "Running mesh transport daemon" }),
    ];
    if service.get("installed").and_then(Value::as_bool) == Some(true) {
        rows.push(json!({
            "id": "service",
            "label": "Always On service payload",
            "current": service.get("payload_version"),
            "pinned": app_pin,
            "detail": "Binary installed in the operating-system service"
        }));
    }
    if artifacts
        .iter()
        .any(|a| a.component == "cli" && a.installed)
    {
        rows.push(json!({ "id": "cli", "label": "AllMyStuff CLI", "current": artifact("cli"), "pinned": app_pin, "detail": "Command-line launcher" }));
    }
    if artifacts
        .iter()
        .any(|a| a.component == "amst" && a.installed)
    {
        rows.push(json!({ "id": "amst", "label": "AMSTerm", "current": artifact("amst"), "pinned": app_pin, "detail": "Standalone mesh terminal" }));
    }
    Ok(json!({ "rows": rows }))
}

/// Repair one row without making the user infer which updater/service owns it.
/// The release updater intentionally reconciles all installed AllMyStuff
/// siblings together; selecting one row still repairs that row plus any other
/// skew it discovers in the same pass.
#[tauri::command]
async fn component_repair(state: State<'_, AppState>, component: String) -> Result<Value, String> {
    match component.as_str() {
        "gui" | "cli" | "amst" => serde_json::to_value(
            allmystuff_updater::update_now()
                .await
                .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string()),
        "serve" => state
            .node
            .request(
                "request_update",
                json!({ "minimum": env!("CARGO_PKG_VERSION") }),
            )
            .await
            .map_err(|e| e.to_string()),
        "service" => service_install(state).await,
        "myownmesh" => {
            let installed_service = tokio::task::spawn_blocking(|| {
                allmystuff_service::status_value(false)
                    .ok()
                    .and_then(|v| v.get("installed").and_then(Value::as_bool))
                    == Some(true)
            })
            .await
            .unwrap_or(false);
            if installed_service {
                allmystuff_node::daemon_spawn::repair_installed_daemon()
                    .await
                    .map_err(|e| e.to_string())?;
                return service_install(state).await;
            } else {
                allmystuff_node::daemon_spawn::repair_installed_daemon()
                    .await
                    .map_err(|e| e.to_string())?;
            }
            state
                .node
                .request("restart_self", json!({}))
                .await
                .map_err(|e| e.to_string())
        }
        other => Err(format!("unknown component: {other}")),
    }
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

/// Apply any staged self-update to disk and relaunch into it. Applying
/// *before* the restart is what makes the relaunch land on the new version in
/// one step: a bare restart would re-exec the still-old binary and only swap
/// it in on the *following* boot (the running image keeps its old inode).
/// Errors only when the required CLI half couldn't be swapped — the staged
/// marker is kept so a later try can succeed; otherwise this never returns,
/// because the process restarts.
#[tauri::command]
async fn update_relaunch(app: tauri::AppHandle) -> Result<(), String> {
    let applied = allmystuff_updater::apply_now().map_err(|e| e.to_string())?;
    #[cfg(target_os = "macos")]
    if applied.is_some() {
        // launchd caches a lightweight code requirement for a LaunchAgent's
        // executable. Portable updates replace our ad-hoc-signed Mach-O and
        // therefore change its cdhash; schedule a registration refresh before
        // exiting or the next login launch is rejected with
        // OS_REASON_CODESIGNING. A failed refresh must not strand an otherwise
        // successfully applied update: startup diagnostics retry it below.
        match schedule_macos_autostart_refresh(&app) {
            Ok(true) => {
                // The detached launchd helper survives this process exiting,
                // refreshes the old job, and lets its RunAtLoad start the new
                // binary. Calling `restart` here as well would race it.
                app.exit(0);
                return Ok(());
            }
            Ok(false) => {}
            Err(e) => tracing::warn!("couldn't refresh Start with computer after update: {e}"),
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = applied;
    app.restart()
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

/// Wi-Fi networks visible to this computer. This gives a headless KVM a
/// useful picker even when the appliance is not online yet or cannot scan.
#[tauri::command]
fn host_wifi_scan() -> Value {
    serde_json::to_value(host_wifi::scan()).unwrap_or(Value::Null)
}

/// Call a NanoKVM JSON endpoint through its loopback site tunnel. Keeping this
/// outside the webview avoids cross-origin preflights and opaque CORS errors.
#[tauri::command]
async fn kvm_api(
    port: u16,
    path: String,
    method: Option<String>,
    body: Option<Value>,
    timeout_ms: Option<u64>,
) -> Result<Value, String> {
    if !path.starts_with('/') || path.starts_with("//") {
        return Err(format!("invalid device path {path}"));
    }
    let url = format!("http://127.0.0.1:{port}{path}");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(timeout_ms.unwrap_or(12_000)))
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("couldn't start the request: {e}"))?;

    let verb = method.as_deref().unwrap_or("GET").to_ascii_uppercase();
    let mut request = match verb.as_str() {
        "GET" => client.get(&url),
        "POST" => client.post(&url),
        "DELETE" => client.delete(&url),
        other => return Err(format!("unsupported method {other}")),
    };
    if let Some(body) = body {
        request = request.json(&body);
    }

    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            let (kind, message) = if error.is_timeout() {
                ("timeout", "the KVM didn't answer in time".to_string())
            } else if error.is_connect() {
                (
                    "connect",
                    "couldn't reach the KVM's console tunnel".to_string(),
                )
            } else {
                ("other", format!("couldn't reach the KVM: {error}"))
            };
            return Ok(json!({
                "status": 0,
                "body": Value::Null,
                "error": { "kind": kind, "message": message },
            }));
        }
    };

    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    Ok(json!({
        "status": status,
        "body": serde_json::from_str::<Value>(&text).ok(),
        "error": Value::Null,
    }))
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

// ---- "Always On" tab: background service (in-process) ------------------
//
// Service management lives in the shared `allmystuff_service` crate, so the GUI
// drives it directly — there is no separate `allmystuff` binary to find, and
// nothing degrades when one isn't around. Status and the unix (per-user)
// mutations run in-process, needing no privilege. Windows services need admin,
// so the GUI re-launches *its own* binary elevated (`--service-do <verb>`,
// handled in `main`) — still no external CLI.

/// The OS background-service status as JSON (`installed` / `running` /
/// `enabled` / `supported` / `manager` / …). Computed in-process by the shared
/// crate; `spawn_blocking` because probing the live state shells out to
/// systemctl/launchctl/sc. Whether the platform *has* a service layer is a
/// static fact — true on all three desktop OSes — so `supported` is only false
/// on a platform the crate doesn't manage at all.
#[tauri::command]
async fn service_status() -> Result<Value, String> {
    tokio::task::spawn_blocking(|| {
        allmystuff_service::status_value(false)
            .unwrap_or_else(|_| json!({ "platform": std::env::consts::OS, "supported": false }))
    })
    .await
    .map_err(|e| format!("service status task failed: {e}"))
}

/// Map a UI verb to the shared crate's command (user scope; Windows ignores it).
fn service_cmd(verb: &str) -> Option<allmystuff_service::ServiceCmd> {
    use allmystuff_service::ServiceCmd;
    Some(match verb {
        "install" => ServiceCmd::Install { log: None },
        "start" => ServiceCmd::Start,
        "stop" => ServiceCmd::Stop,
        "restart" => ServiceCmd::Restart,
        "uninstall" => ServiceCmd::Uninstall,
        _ => return None,
    })
}

/// The verb after a `--service-do` flag in this process's argv, if any (the
/// elevated Windows self-invocation; see `main`).
fn service_do_verb() -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    let i = args.iter().position(|a| a == "--service-do")?;
    args.get(i + 1).cloned()
}

fn process_arg_value(flag: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    let i = args.iter().position(|arg| arg == flag)?;
    args.get(i + 1).cloned()
}

/// Run a service mutation off the UI thread (it shells out to the init system,
/// and on Windows waits on an elevated child). Returns `{ ok, output }`.
async fn service_mutate(verb: &'static str) -> Result<Value, String> {
    tokio::task::spawn_blocking(move || service_mutate_blocking(verb))
        .await
        .map_err(|e| format!("service {verb} task failed: {e}"))?
}

/// Unix: install/manage the per-user service in-process — no privilege, no CLI.
#[cfg(not(windows))]
fn service_mutate_blocking(verb: &str) -> Result<Value, String> {
    let cmd = service_cmd(verb).ok_or_else(|| format!("unknown service action: {verb}"))?;
    match allmystuff_service::run(false, cmd) {
        Ok(()) => Ok(json!({ "ok": true, "output": format!("service {verb}: done") })),
        Err(e) => Ok(json!({ "ok": false, "output": format!("{e:#}") })),
    }
}

/// Windows: a service needs admin, so re-launch our own binary elevated to do
/// the work (`--service-do <verb>`, handled in `main`). Still no external CLI;
/// the elevated child runs in its own console, so we report by exit code and
/// let the UI re-read status.
#[cfg(windows)]
fn service_mutate_blocking(verb: &str) -> Result<Value, String> {
    let exe = std::env::current_exe().map_err(|e| format!("locating AllMyStuff: {e}"))?;
    let exe = exe.to_string_lossy().replace('\'', "''");
    let home = dirs::home_dir()
        .ok_or_else(|| "couldn't resolve the current Windows profile".to_string())?
        .to_string_lossy()
        .replace('\'', "''");
    let sid = current_windows_user_sid()?.replace('\'', "''");
    // Resolve the daemon while we still have the desktop user's exact PATH
    // and bundle context, then carry that absolute path across UAC.  The
    // elevated child may see a different PATH; guessing there is what produced
    // a service whose local node answered while no mesh daemon was running.
    let mesh_arg = if verb == "install" {
        let (mesh, _) = allmystuff_node::daemon_spawn::find_daemon_binary()
            .map_err(|e| format!("locating MyOwnMesh for Always On: {e:#}"))?;
        format!(" --service-mesh \"{}\"", mesh.to_string_lossy())
    } else {
        String::new()
    };
    let elevated_args =
        format!("--service-do {verb} --service-home \"{home}\" --service-sid {sid}{mesh_arg}")
            .replace('\'', "''");
    let ps = format!(
        "try {{ $p = Start-Process -FilePath '{exe}' -ArgumentList '{elevated_args}' \
         -Verb RunAs -Wait -PassThru -WindowStyle Hidden; exit $p.ExitCode }} \
         catch {{ exit 1223 }}"
    );
    // CREATE_NO_WINDOW: the GUI has no console, so a bare `powershell` spawn
    // would flash one for the frame it runs (the elevated child is already
    // hidden via `-WindowStyle Hidden`). Matches the flag the service crate
    // sets on its own Windows spawns.
    use std::os::windows::process::CommandExt as _;
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &ps])
        .creation_flags(0x0800_0000)
        .output()
        .map_err(|e| format!("launching elevated AllMyStuff: {e}"))?;
    let code = out.status.code().unwrap_or(-1);
    if code == 1223 {
        // ERROR_CANCELLED — the user declined the UAC prompt.
        return Err("Administrator approval was declined.".to_string());
    }
    Ok(json!({
        "ok": code == 0,
        "output": if code == 0 {
            format!("service {verb}: done")
        } else {
            format!("service {verb} failed (exit {code})")
        },
    }))
}

#[cfg(windows)]
fn current_windows_user_sid() -> Result<String, String> {
    use std::os::windows::process::CommandExt as _;
    let out = std::process::Command::new("whoami")
        .args(["/user", "/fo", "csv", "/nh"])
        .creation_flags(0x0800_0000)
        .output()
        .map_err(|e| format!("reading the current Windows account SID: {e}"))?;
    if !out.status.success() {
        return Err("couldn't read the current Windows account SID".into());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let sid = text
        .split([',', '"', '\r', '\n'])
        .map(str::trim)
        .find(|part| part.starts_with("S-1-"))
        .ok_or_else(|| "Windows returned no account SID".to_string())?;
    Ok(sid.to_string())
}

/// Transfer the one-machine node socket from a GUI-owned child to a service
/// operation that starts a replacement. If the service fails to answer, put
/// the transient node back so this machine does not disappear from the mesh.
async fn service_start_with_handoff(
    state: State<'_, AppState>,
    verb: &'static str,
) -> Result<Value, String> {
    state.node_child.lock().take();
    let result = service_mutate(verb).await;
    let started = result
        .as_ref()
        .ok()
        .and_then(|value| value.get("ok"))
        .and_then(Value::as_bool)
        == Some(true);
    if started && !wait_for_node_ready().await {
        tracing::warn!("service {verb} completed but its node did not become ready");
    }
    if !started || !NodeClient::probe().await {
        if let Ok(Some(child)) = ensure_node_running().await {
            state.node_child.lock().install(child);
        }
    }
    result
}

#[tauri::command]
async fn service_install(state: State<'_, AppState>) -> Result<Value, String> {
    service_start_with_handoff(state, "install").await
}
#[tauri::command]
async fn service_start(state: State<'_, AppState>) -> Result<Value, String> {
    service_start_with_handoff(state, "start").await
}
#[tauri::command]
async fn service_stop() -> Result<Value, String> {
    service_mutate("stop").await
}
#[tauri::command]
async fn service_restart(state: State<'_, AppState>) -> Result<Value, String> {
    service_start_with_handoff(state, "restart").await
}
#[tauri::command]
async fn service_uninstall() -> Result<Value, String> {
    service_mutate("uninstall").await
}

/// Local development diagnostics only. The preference is read by the node at
/// logging initialization, so changing it is deliberately restart-scoped and
/// never sends anything through the mesh/control or signaling planes.
#[tauri::command]
fn debug_logging_get() -> bool {
    allmystuff_node::diagnostics::debug_logging_enabled()
}

#[tauri::command]
fn debug_logging_set(enabled: bool) -> Result<bool, String> {
    allmystuff_node::diagnostics::set_debug_logging(enabled).map_err(|e| e.to_string())?;
    Ok(allmystuff_node::diagnostics::debug_logging_enabled())
}

/// The persisted "Always On" window/startup behaviour (close/minimize to tray,
/// start minimized).
#[tauri::command]
fn window_behavior_get(wb: State<'_, window_behavior::WindowBehavior>) -> Value {
    behavior_json(wb.get())
}

#[tauri::command]
fn window_behavior_set(
    wb: State<'_, window_behavior::WindowBehavior>,
    close_to_tray: bool,
    minimize_to_tray: bool,
    start_minimized: bool,
) -> Value {
    // Preserve the internal autostart-default marker — it isn't a user field.
    let autostart_defaulted = wb.get().autostart_defaulted;
    behavior_json(wb.set(window_behavior::Behavior {
        close_to_tray,
        minimize_to_tray,
        start_minimized,
        autostart_defaulted,
    }))
}

fn behavior_json(b: window_behavior::Behavior) -> Value {
    json!({
        "close_to_tray": b.close_to_tray,
        "minimize_to_tray": b.minimize_to_tray,
        "start_minimized": b.start_minimized,
    })
}

/// Whether "Start with computer" (the OS login item) is currently registered.
#[tauri::command]
fn autostart_get(app: tauri::AppHandle) -> bool {
    app.autolaunch().is_enabled().unwrap_or(false)
}

/// Register / unregister the login item, returning the resulting state.
#[tauri::command]
fn autostart_set(app: tauri::AppHandle, enabled: bool) -> Result<bool, String> {
    let mgr = app.autolaunch();
    if enabled {
        mgr.enable().map_err(|e| e.to_string())?;
    } else {
        mgr.disable().map_err(|e| e.to_string())?;
    }
    Ok(mgr.is_enabled().unwrap_or(enabled))
}

#[cfg(target_os = "macos")]
const MACOS_AUTOSTART_LABEL: &str = "AllMyStuff";

/// launchd says this when the executable at a loaded LaunchAgent path no
/// longer satisfies the lightweight code requirement cached at registration.
/// Portable updates intentionally replace that executable in place.
#[cfg(any(target_os = "macos", test))]
fn macos_autostart_needs_refresh(status: &str) -> bool {
    status.contains("needs LWCR update") || status.contains("OS_REASON_CODESIGNING")
}

#[cfg(target_os = "macos")]
fn macos_autostart_plist() -> Result<std::path::PathBuf, String> {
    dirs::home_dir()
        .map(|home| {
            home.join("Library")
                .join("LaunchAgents")
                .join(format!("{MACOS_AUTOSTART_LABEL}.plist"))
        })
        .ok_or_else(|| "couldn't resolve the macOS home directory".to_string())
}

#[cfg(target_os = "macos")]
fn macos_update_relaunch_marker() -> Result<std::path::PathBuf, String> {
    dirs::home_dir()
        .map(|home| {
            home.join(".allmystuff")
                .join("updates")
                .join("show-after-autostart-refresh")
        })
        .ok_or_else(|| "couldn't resolve the macOS home directory".to_string())
}

#[cfg(target_os = "macos")]
fn launchctl_status_for(label: &str) -> Result<String, String> {
    let uid = std::process::Command::new("/usr/bin/id")
        .arg("-u")
        .output()
        .map_err(|e| format!("reading the macOS user id: {e}"))?;
    if !uid.status.success() {
        return Err("`id -u` failed while locating the macOS launchd domain".into());
    }
    let uid = String::from_utf8_lossy(&uid.stdout).trim().to_string();
    let output = std::process::Command::new("launchctl")
        .args(["print", &format!("gui/{uid}/{label}")])
        .output()
        .map_err(|e| format!("querying the macOS login item: {e}"))?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok(text)
}

/// Submit a one-shot helper as its own launchd job. A process cannot unload
/// the job that owns it and then continue to reload that job: launchd kills
/// the process tree during `unload`. The separate helper waits for this GUI to
/// exit, refreshes the registration, and RunAtLoad starts the updated GUI.
#[cfg(target_os = "macos")]
fn schedule_macos_autostart_refresh(app: &tauri::AppHandle) -> Result<bool, String> {
    let mgr = app.autolaunch();
    if !mgr.is_enabled().map_err(|e| e.to_string())? {
        return Ok(false);
    }

    // Rewrite the plugin-owned plist while this process is still alive. The
    // helper only has to atomically swap launchd's in-memory registration.
    mgr.enable().map_err(|e| e.to_string())?;
    let exe = std::env::current_exe().map_err(|e| format!("locating AllMyStuff: {e}"))?;
    let pid = std::process::id().to_string();
    let label = format!("com.allmystuff.autostart-refresh-{pid}");
    let submitted = std::process::Command::new("launchctl")
        .args(["submit", "-l", &label, "--"])
        .arg(exe)
        .args(["--macos-autostart-refresh", &pid])
        .status()
        .map_err(|e| format!("submitting the macOS login-item refresh helper: {e}"))?;
    if !submitted.success() {
        return Err(format!(
            "launchctl could not submit the refresh helper ({submitted})"
        ));
    }
    Ok(true)
}

#[cfg(target_os = "macos")]
fn run_macos_autostart_refresh(parent_pid: &str) -> Result<(), String> {
    use std::process::Stdio;

    for _ in 0..200 {
        let parent_alive = std::process::Command::new("/bin/kill")
            .args(["-0", parent_pid])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if !parent_alive {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let parent_alive = std::process::Command::new("/bin/kill")
        .args(["-0", parent_pid])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if parent_alive {
        return Err("the previous AllMyStuff process did not exit within 10 seconds".into());
    }

    let plist = macos_autostart_plist()?;
    let unload = std::process::Command::new("launchctl")
        .args(["unload", "-w"])
        .arg(&plist)
        .status()
        .map_err(|e| format!("unloading {}: {e}", plist.display()))?;
    if !unload.success() {
        return Err(format!(
            "launchctl could not unload {} ({unload})",
            plist.display()
        ));
    }

    let marker = macos_update_relaunch_marker()?;
    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    std::fs::write(&marker, []).map_err(|e| format!("writing {}: {e}", marker.display()))?;
    let load = std::process::Command::new("launchctl")
        .args(["load", "-w"])
        .arg(&plist)
        .status()
        .map_err(|e| format!("loading {}: {e}", plist.display()))?;
    if !load.success() {
        let _ = std::fs::remove_file(marker);
        return Err(format!(
            "launchctl could not load {} ({load})",
            plist.display()
        ));
    }
    Ok(())
}

/// Repair users updating from a build that did not refresh launchd itself.
/// The new executable can still be reached by the updater's direct relaunch;
/// once running, this clears the stale LWCR before the next login.
#[cfg(target_os = "macos")]
fn repair_macos_autostart_if_needed(app: &tauri::AppHandle) -> bool {
    let Ok(status) = launchctl_status_for(MACOS_AUTOSTART_LABEL) else {
        return false;
    };
    if !macos_autostart_needs_refresh(&status) {
        return false;
    }
    match schedule_macos_autostart_refresh(app) {
        Ok(true) => {
            tracing::info!("scheduled repair of the macOS Start with computer registration");
            true
        }
        Ok(false) => false,
        Err(e) => {
            tracing::warn!("couldn't repair the macOS Start with computer item: {e}");
            false
        }
    }
}

/// Build the system-tray / menu-bar icon — the home AllMyStuff keeps while
/// "Always On" hides its window. Left-click (or "Show AllMyStuff") brings the
/// main window back; "Quit" exits for real.
fn build_tray(app: &tauri::AppHandle) -> tauri::Result<()> {
    use tauri::menu::{MenuBuilder, MenuItemBuilder};
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

    let show = MenuItemBuilder::with_id("show", "Show AllMyStuff").build(app)?;
    let quit = MenuItemBuilder::with_id("quit", "Quit AllMyStuff").build(app)?;
    let menu = MenuBuilder::new(app).items(&[&show, &quit]).build()?;

    let mut builder = TrayIconBuilder::with_id("main")
        .tooltip("AllMyStuff")
        .menu(&menu)
        // Left-click reveals the window; the menu rides the right-click.
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => reveal_main_window(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                reveal_main_window(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    Ok(())
}

/// Bring the main window back from the tray (or a minimized state) and focus it.
fn reveal_main_window(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

/// Bring the per-machine node back up if it has gone away, storing the child we
/// spawn so it still dies with the app. Safe to call any time:
/// [`ensure_node_running`] probes the control socket first and returns `None`
/// when a node already answers, so a healthy node is left untouched.
///
/// Called on a single-instance hand-off. A second launch is usually the user
/// re-opening the app, but it can also be `amst` opening it expressly to get a
/// node onto the mesh — so as well as revealing the window we make sure the node
/// is actually running, healing one that died under a still-running app (a node
/// crash, or a reused Always-On service node that bounced) instead of leaving a
/// live app with no node behind it.
fn heal_node(app: &tauri::AppHandle) {
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        // Never heal over our own live serve — replacing its handle would
        // kill it (see the pump's wedge handling).
        if handle.state::<AppState>().node_child.lock().is_alive() {
            return;
        }
        match ensure_node_running().await {
            Ok(Some(child)) => {
                let generation = handle.state::<AppState>().node_child.lock().install(child);
                tracing::info!(generation, "installed GUI-owned node during heal");
            }
            Ok(None) => {}
            Err(e) => tracing::error!("couldn't bring the allmystuff node back up: {e:#}"),
        }
    });
}

/// Give a newly installed Windows service time to launch its console-session
/// agent and bind the shared control pipe before considering a GUI fallback.
async fn wait_for_node_ready() -> bool {
    for _ in 0..50 {
        if NodeClient::probe().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    false
}

/// Apply the persisted startup preferences once the app is built: reveal the
/// main window unless this is a login-item launch the user asked to start
/// minimized, and — on a fresh install — default "Start with computer" on.
fn apply_startup_behavior(app: &tauri::AppHandle) {
    let wb = app.state::<window_behavior::WindowBehavior>();

    // The main window is created hidden (tauri.conf `visible: false`) so a
    // start-minimized launch never flashes. Show it now unless we should stay
    // hidden: a `--minimized` autostart launch with the pref on.
    let launched_minimized = std::env::args().any(|a| a == "--minimized");
    #[cfg(target_os = "macos")]
    let show_after_update = macos_update_relaunch_marker()
        .ok()
        .is_some_and(|marker| marker.exists() && std::fs::remove_file(marker).is_ok());
    #[cfg(not(target_os = "macos"))]
    let show_after_update = false;
    let start_hidden = launched_minimized && wb.start_minimized() && !show_after_update;
    if start_hidden {
        tracing::info!("starting minimized to the tray (login item)");
    } else if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.set_focus();
    }

    // First launch on this install: default "Start with computer" on, once, so
    // a later user opt-out is never undone.
    if wb.needs_autostart_default() {
        match app.autolaunch().enable() {
            Ok(()) => tracing::info!("enabled Start with computer (install default)"),
            Err(e) => tracing::warn!("couldn't enable Start with computer by default: {e}"),
        }
        wb.mark_autostart_defaulted();
    }
}

/// Subscribe to the node's event stream and re-emit each event on Tauri's bus,
/// so the Svelte front-end sees exactly what it used to when the engine ran
/// in-process. Reconnects if the node restarts.
async fn run_event_pump(app: tauri::AppHandle, node: Arc<NodeClient>) {
    use tokio::sync::mpsc;
    let mut recovery = BackendRecovery::default();
    loop {
        // The node may be *gone*, not just restarting — e.g. another client app
        // (CEC Support) spawned it and exited, taking the kill-on-close serve
        // with it. A client doesn't require whichever app brought the engine
        // up: if nothing answers the socket, respawn it ourselves. Probe with
        // patience first — a serve that is *starting* (spawned, socket not
        // bound yet) must not read as "gone": respawning over it would
        // kill-on-drop the very child being waited on, and the stack would
        // flap spawn/kill forever.
        let first_probe_ready = NodeClient::probe().await;
        let mut socket = if first_probe_ready {
            RecoverySocketState::Ready
        } else {
            RecoverySocketState::Unresponsive
        };
        if !first_probe_ready {
            for _ in 0..50 {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                if NodeClient::probe().await {
                    socket = RecoverySocketState::ReadyAfterGrace;
                    break;
                }
            }
        }

        let (ownership, generation) = app.state::<AppState>().node_child.lock().observation();
        let evidence = recovery.observe(RecoveryObservation {
            socket,
            ownership,
            generation,
        });
        match evidence.decision {
            RecoveryDecision::Healthy => {
                tracing::debug!(
                    target: "allmystuff::backend_recovery",
                    socket = evidence.observation.socket.label(),
                    ownership = evidence.observation.ownership.label(),
                    generation = evidence.observation.generation,
                    prior_wedged_rounds = evidence.prior_wedged_rounds,
                    wedged_rounds = evidence.wedged_rounds,
                    decision = evidence.decision.label(),
                    "local node recovery decision"
                );
            }
            RecoveryDecision::StartupCompleted => {
                tracing::info!(
                    target: "allmystuff::backend_recovery",
                    socket = evidence.observation.socket.label(),
                    ownership = evidence.observation.ownership.label(),
                    generation = evidence.observation.generation,
                    decision = evidence.decision.label(),
                    "local node became ready during the existing startup grace window"
                );
            }
            RecoveryDecision::WaitForOwnedNode => {
                tracing::warn!(
                    target: "allmystuff::backend_recovery",
                    socket = evidence.observation.socket.label(),
                    ownership = evidence.observation.ownership.label(),
                    generation = evidence.observation.generation,
                    wedged_rounds = evidence.wedged_rounds,
                    restart_after = WEDGED_RESTART_ROUNDS,
                    decision = evidence.decision.label(),
                    "local node socket is unresponsive while the GUI-owned process remains alive"
                );
            }
            RecoveryDecision::RestartOwnedNode => {
                tracing::warn!(
                    target: "allmystuff::backend_recovery",
                    socket = evidence.observation.socket.label(),
                    ownership = evidence.observation.ownership.label(),
                    generation = evidence.observation.generation,
                    restart_after = WEDGED_RESTART_ROUNDS,
                    decision = evidence.decision.label(),
                    "restarting the unresponsive GUI-owned node"
                );
                app.state::<AppState>().node_child.lock().take();
                match ensure_node_running().await {
                    Ok(Some(child)) => {
                        let next_generation =
                            app.state::<AppState>().node_child.lock().install(child);
                        tracing::info!(
                            target: "allmystuff::backend_recovery",
                            generation = next_generation,
                            "installed replacement GUI-owned node"
                        );
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!("couldn't bring the node back up: {e:#}"),
                }
            }
            RecoveryDecision::EnsureNode => {
                tracing::info!(
                    target: "allmystuff::backend_recovery",
                    socket = evidence.observation.socket.label(),
                    ownership = evidence.observation.ownership.label(),
                    generation = evidence.observation.generation,
                    decision = evidence.decision.label(),
                    "local node is unavailable; ensuring one is running"
                );
                match ensure_node_running().await {
                    Ok(Some(child)) => {
                        let next_generation =
                            app.state::<AppState>().node_child.lock().install(child);
                        tracing::info!(
                            target: "allmystuff::backend_recovery",
                            generation = next_generation,
                            "installed GUI-owned node"
                        );
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!("couldn't bring the node back up: {e:#}"),
                }
            }
        }
        let (tx, mut rx) = mpsc::channel::<NodeEvent>(256);
        if let Err(e) = node.subscribe_events(tx).await {
            tracing::warn!("node event subscribe failed: {e:#}; retrying");
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            continue;
        }
        // A successful subscription is also the authoritative "this GUI can
        // talk to the current node owner" edge.  The owner may have restarted
        // while the front-end still had `backendConnected = true`; without a
        // new edge the Svelte store never re-scanned this machine and could
        // retain the previous owner's empty/stale local inventory forever.
        // Initial delivery is harmless (startup hydration is idempotent), and
        // every later delivery closes that owner-handoff gap without polling a
        // hardware scan every three seconds.
        let _ = app.emit("allmystuff://backend-ready", json!({}));
        while let Some(ev) = rx.recv().await {
            match ev {
                NodeEvent::Emit { event, payload } => {
                    let _ = app.emit(&event, payload);
                }
                NodeEvent::Upgrade => {
                    match allmystuff_updater::update_now().await {
                        Ok(outcome) => {
                            tracing::info!("fleet GUI upgrade completed: {outcome:?}")
                        }
                        Err(e) => tracing::warn!("fleet GUI upgrade failed: {e}"),
                    }
                    app.restart();
                }
                NodeEvent::Restart => app.restart(), // never returns
            }
        }
        tracing::info!("node event stream ended; resubscribing");
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

/// Native close/minimize handling for the **main** window only (secondary
/// console / terminal / room windows always close normally): honour the
/// persisted "Always On" preference by hiding to the tray instead of closing
/// or minimizing to the taskbar.
fn handle_window_event(window: &tauri::Window, event: &tauri::WindowEvent) {
    if window.label() != "main" {
        return;
    }
    match event {
        tauri::WindowEvent::CloseRequested { api, .. }
            if window
                .state::<window_behavior::WindowBehavior>()
                .close_to_tray() =>
        {
            // Keep the process (and the tray) alive; the window just hides.
            api.prevent_close();
            let _ = window.hide();
        }
        // No portable "minimized" event — catch the resize and check the state.
        tauri::WindowEvent::Resized(_)
            if window
                .state::<window_behavior::WindowBehavior>()
                .minimize_to_tray()
                && window.is_minimized().unwrap_or(false) =>
        {
            let _ = window.hide();
        }
        _ => {}
    }
}

fn main() {
    #[cfg(target_os = "macos")]
    if let Some(parent_pid) = process_arg_value("--macos-autostart-refresh") {
        // Always exit successfully: `launchctl submit` restarts this helper on
        // failure, which would turn a one-shot repair error into a tight loop.
        if let Err(e) = run_macos_autostart_refresh(&parent_pid) {
            eprintln!("AllMyStuff login-item refresh failed: {e}");
        }
        std::process::exit(0);
    }

    #[cfg(windows)]
    if std::env::args().any(|arg| arg == "--service-bootstrap") {
        let verb = process_arg_value("--service-bootstrap").unwrap_or_else(|| "install".into());
        let code = match service_mutate_blocking(&verb) {
            Ok(value) if value.get("ok").and_then(Value::as_bool) == Some(true) => 0,
            Ok(_) => 1,
            Err(e) => {
                eprintln!("AllMyStuff privileged host setup failed: {e}");
                1
            }
        };
        std::process::exit(code);
    }

    // Elevated service action: `<gui-exe> --service-do <verb>`. On Windows the
    // "Always On" tab re-launches this binary elevated to install/manage the
    // service; here we just run the verb in-process and exit, no webview. (The
    // unix path calls the crate directly and never reaches this.)
    if let Some(verb) = service_do_verb() {
        if let Some(home) = process_arg_value("--service-home") {
            std::env::set_var("ALLMYSTUFF_SERVICE_HOME", home);
        }
        if let Some(sid) = process_arg_value("--service-sid") {
            std::env::set_var("ALLMYSTUFF_SERVICE_CLIENT_SID", sid);
        }
        if let Some(mesh) = process_arg_value("--service-mesh") {
            std::env::set_var("ALLMYSTUFF_SERVICE_MESH_BIN", mesh);
        }
        let code = match service_cmd(&verb) {
            Some(cmd) => match allmystuff_service::run(false, cmd) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("allmystuff service {verb}: {e:#}");
                    1
                }
            },
            None => {
                eprintln!("allmystuff: unknown service action `{verb}`");
                2
            }
        };
        std::process::exit(code);
    }

    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    workaround_pi_webkit_rendering();

    let log_level = std::env::var("ALLMYSTUFF_GUI_LOG").unwrap_or_else(|_| {
        if allmystuff_node::diagnostics::debug_logging_enabled() {
            "info,allmystuff_gui=debug,allmystuff_node=debug".to_string()
        } else {
            "info,allmystuff_gui=info".to_string()
        }
    });
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(log_level))
        .with_target(false)
        .init();

    // Apply any update staged on the previous run before anything else — but
    // *after* the tracing subscriber is installed, so a failed swap (e.g. a
    // kept-and-retried CLI half that can't be replaced) is actually logged
    // instead of being dropped into a no-op dispatcher and failing silently
    // on every launch.
    allmystuff_updater::apply_pending_if_any();

    tauri::Builder::default()
        // Keep AllMyStuff to one running copy. A second launch (the user
        // double-clicks the app again, the login item fires while it's already
        // up, `open -n` on macOS) would otherwise stand up a rival process with
        // its own node and `myownmesh` daemon fighting over the same control
        // socket. The single-instance plugin makes that second launch hand off
        // to the first and exit; the callback runs *in the original instance*,
        // so we bring its window back to the front — and re-ensure the node,
        // since a second launch may be `amst` opening the app to get one (this
        // heals a node that died under a still-running app). Must be registered
        // before any other plugin for the guard to take effect.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            reveal_main_window(app);
            heal_node(app);
        }))
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        // Terminal copy/paste: the async clipboard API is unreliable in
        // WebKitGTK, so the terminal windows use the plugin instead.
        .plugin(tauri_plugin_clipboard_manager::init())
        // "Start with computer". The login item launches us with `--minimized`;
        // whether that actually starts hidden is gated on the user's
        // start-minimized preference at startup (see `setup`), so the arg can
        // ride along unconditionally.
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--minimized"]),
        ))
        .manage(window_behavior::WindowBehavior::load())
        .on_window_event(handle_window_event)
        .invoke_handler(tauri::generate_handler![
            scan_self,
            scan_full,
            connect_route,
            drive_map,
            drive_map_from,
            drive_mappings,
            drive_unmap,
            folder_share_from,
            folder_open,
            folder_open_on,
            kvm_media_stage,
            kvm_media_unmount,
            disconnect_route,
            client_log,
            claim_node,
            upgrade_node,
            restart_node,
            restart_app,
            restart_device,
            refresh_node,
            set_claimable,
            set_public_claims,
            claim_via_code,
            kvm_attach,
            kvm_detach,
            kvm_mesh_add,
            kvm_mesh_remove,
            share_grant,
            fleet_storage_status,
            fleet_storage_set_policy,
            fleet_storage_set_allocation,
            fleet_storage_set_device_role,
            share_revoke,
            share_stop,
            send_input,
            clipboard_paste,
            clipboard_drop,
            clipboard_pull,
            video_watch,
            video_poll,
            video_unwatch,
            video_refresh,
            video_feedback,
            tune_route,
            route_dials,
            labs_set,
            open_console_window,
            open_video_window,
            term_send,
            term_watch,
            fleet_service_profiles,
            fleet_storage_local_volumes,
            term_poll,
            term_unwatch,
            terminal_sessions,
            open_terminal_window,
            file_send,
            local_file_transfer_scan,
            local_file_transfer_start,
            local_file_transfer_cancel,
            local_file_transfer_operations,
            local_file_operation_dismiss,
            fleetfiles_local_desktop,
            file_watch,
            file_poll,
            file_unwatch,
            file_download,
            file_download_cancel,
            open_files_window,
            open_files_workspace_window,
            files_namespace_adopt,
            files_namespace_mutate,
            files_namespace_list,
            files_canvas_snapshot,
            files_canvas_status,
            files_canvas_apply,
            files_canvas_purge_tombstones,
            local_file_locations,
            local_file_list,
            local_directory_watch,
            local_directory_unwatch,
            local_file_icon,
            local_file_preview,
            local_file_open,
            local_file_context_menu,
            local_file_mkdir,
            local_file_rename,
            local_file_trash,
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
            reset_networking,
            factory_reset,
            fleet_kick,
            fleet_set_name,
            fleet_grant_role,
            fleet_revoke_role,
            fleet_set_hubs,
            fleet_mfa_status,
            fleet_mfa_enroll,
            fleet_mfa_disable,
            cec_status,
            cec_dial,
            cec_dial_node,
            open_cec_window,
            open_chat_window,
            cec_pending,
            cec_approve,
            cec_deny,
            cec_revoke,
            cec_grants,
            cec_chat_send,
            cec_chat_history,
            cec_dialed,
            cec_help_list,
            cec_help_watch,
            cec_cancel_dial,
            forget_node,
            mesh_status,
            mesh_identity,
            mesh_networks,
            mesh_peers,
            link_status,
            mesh_network_add,
            mesh_network_remove,
            mesh_network_update,
            disabled_networks,
            network_set_enabled,
            network_reconnect,
            mesh_config_show,
            mesh_network_export_file,
            mesh_network_id_generate,
            mesh_roster_approve,
            mesh_roster_remove,
            mesh_roster_list,
            mesh_identity_set_label,
            update_status,
            component_status,
            component_repair,
            update_check,
            update_apply,
            update_relaunch,
            update_set_prefs,
            update_latest_version,
            host_wifi_scan,
            kvm_api,
            service_status,
            service_install,
            service_start,
            service_stop,
            service_restart,
            service_uninstall,
            debug_logging_get,
            debug_logging_set,
            window_behavior_get,
            window_behavior_set,
            autostart_get,
            autostart_set,
        ])
        .setup(move |app| {
            // The tray icon is what keeps AllMyStuff reachable once "Always On"
            // hides its window to the notification area / menu bar.
            if let Err(e) = build_tray(app.handle()) {
                tracing::warn!("couldn't create the tray icon: {e}");
            }
            apply_startup_behavior(app.handle());
            #[cfg(target_os = "macos")]
            if repair_macos_autostart_if_needed(app.handle()) {
                // The detached helper will relaunch through the repaired
                // RunAtLoad job. Stop setup before we spawn another node.
                app.handle().exit(0);
                return Ok(());
            }
            let handle = app.handle().clone();
            let node = match NodeClient::new() {
                Ok(n) => Arc::new(n),
                Err(e) => {
                    tracing::error!("couldn't resolve the node socket: {e:#}");
                    return Err(e.into());
                }
            };
            app.manage(AppState {
                node: node.clone(),
                node_child: Mutex::new(OwnedNode::default()),
                local_files: Arc::new(Mutex::new(LocalFileBrowser::default())),
                local_directory_watchers: Arc::new(Mutex::new(HashMap::new())),
                next_local_directory_watch: AtomicU64::new(1),
            });
            // Installer hooks cover new NSIS installs. Existing installations
            // can arrive here through the self-updater, so migrate the old
            // Session-0 service exactly once when its ImagePath lacks the new
            // console-session host arguments. Development builds never prompt.
            #[cfg(all(windows, not(debug_assertions)))]
            let migrate_privileged_host = {
                let status = allmystuff_service::status_value(false).unwrap_or_default();
                status
                    .get("privileged_host_current")
                    .and_then(Value::as_bool)
                    != Some(true)
            };
            #[cfg(not(all(windows, not(debug_assertions))))]
            let migrate_privileged_host = false;
            tauri::async_runtime::spawn(async move {
                #[cfg(all(windows, not(debug_assertions)))]
                let mut service_repair_attempted = false;
                // Migrate first. Starting a temporary GUI node concurrently
                // made older installs race the old service and replacement
                // service for the machine control pipe.
                if migrate_privileged_host {
                    #[cfg(all(windows, not(debug_assertions)))]
                    {
                        service_repair_attempted = true;
                    }
                    let migrated =
                        match tokio::task::spawn_blocking(|| service_mutate_blocking("install"))
                            .await
                        {
                            Ok(Ok(value))
                                if value.get("ok").and_then(Value::as_bool) == Some(true) =>
                            {
                                tracing::info!("installed the privileged interactive Windows host");
                                true
                            }
                            Ok(Ok(_)) => {
                                tracing::warn!("privileged Windows host setup did not complete");
                                false
                            }
                            Ok(Err(e)) => {
                                tracing::warn!("privileged Windows host setup failed: {e}");
                                false
                            }
                            Err(e) => {
                                tracing::warn!("privileged Windows host setup task failed: {e}");
                                false
                            }
                        };
                    if migrated && !wait_for_node_ready().await {
                        tracing::warn!(
                            "migrated Windows host did not become ready; starting the GUI fallback"
                        );
                    }
                }
                // One node per machine: reuse the Always-On service's node if
                // it's up, else spawn a transient one tied to this app's
                // lifetime. The node owns the Mesh and supervises the myownmesh
                // daemon itself — the GUI no longer runs either.
                match ensure_node_running().await {
                    Ok(child) => {
                        if let Some(c) = child {
                            let generation =
                                handle.state::<AppState>().node_child.lock().install(c);
                            tracing::info!(
                                target: "allmystuff::backend_recovery",
                                generation,
                                "installed initial GUI-owned node"
                            );
                        }
                    }
                    Err(e) => tracing::error!("couldn't bring up the allmystuff node: {e:#}"),
                }

                // `ensure_node_running` first asks the process that owns the
                // socket to update its own installation. That is the normal,
                // no-UAC path and correctly reaches a protected service copy.
                // A service from before the request-update contract cannot do
                // that once; repair it from the current bundle, then every
                // later release converges through the node request above.
                #[cfg(all(windows, not(debug_assertions)))]
                {
                    let running_current =
                        running_node_satisfies(env!("CARGO_PKG_VERSION")).await;
                    let service_status =
                        allmystuff_service::status_value(false).unwrap_or_default();
                    let installed =
                        service_status.get("installed").and_then(Value::as_bool) == Some(true);
                    let service_payload_current = service_status
                        .get("payload_current")
                        .and_then(Value::as_bool)
                        == Some(true);
                    if installed
                        && (!running_current || !service_payload_current)
                        && !service_repair_attempted
                    {
                        tracing::warn!(
                            running_current,
                            service_payload_current,
                            "the installed Windows node did not converge through dependency update requests; repairing the service payload once"
                        );
                        // A transient GUI-owned fallback must release the one
                        // node pipe before the elevated replacement starts.
                        handle.state::<AppState>().node_child.lock().take();
                        match tokio::task::spawn_blocking(|| service_mutate_blocking("install"))
                            .await
                        {
                            Ok(Ok(value))
                                if value.get("ok").and_then(Value::as_bool) == Some(true) =>
                            {
                                if wait_for_node_ready().await
                                    && running_node_satisfies(env!("CARGO_PKG_VERSION")).await
                                {
                                    tracing::info!(
                                        "legacy Windows service repaired and verified at the current AllMyStuff version"
                                    );
                                } else {
                                    tracing::warn!(
                                        "the repaired Windows service did not return at the required AllMyStuff version"
                                    );
                                }
                            }
                            Ok(Ok(_)) => tracing::warn!("legacy Windows service repair failed"),
                            Ok(Err(error)) => {
                                tracing::warn!("legacy Windows service repair failed: {error}")
                            }
                            Err(error) => {
                                tracing::warn!("legacy Windows service repair task failed: {error}")
                            }
                        }
                    }
                }
                run_event_pump(handle, node).await;
            });
            // Self-update ticker — the first check fires shortly after launch,
            // then at the configured interval. Spawned unconditionally:
            // `check_now` no-ops when auto-update is off. Without this the
            // in-app updater only ever checks when the user clicks "Check now".
            //
            // Every outcome is forwarded to the webview as `update://checked`,
            // so a release found in the background actually reaches the user.
            // The ticker used to run mute — a staged update sat on disk until
            // someone happened to open Settings → Updates, which is what made
            // an app that *was* checking look like one that never did.
            let update_handle = app.handle().clone();
            tauri::async_runtime::spawn(allmystuff_updater::tick_forever_notify(move |outcome| {
                match serde_json::to_value(outcome) {
                    Ok(payload) => {
                        if let Err(e) = update_handle.emit("update://checked", payload) {
                            tracing::warn!("couldn't emit the self-update outcome: {e}");
                        }
                    }
                    Err(e) => tracing::warn!("couldn't serialise the self-update outcome: {e}"),
                }
            }));
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building the AllMyStuff GUI")
        .run(|app, event| {
            if let RunEvent::Exit = event {
                // Kill the node we spawned (if any). A reused Always-On service
                // node has no child here and keeps running, so the machine
                // stays reachable.
                app.state::<AppState>().node_child.lock().take();
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_autostart_refresh_detects_stale_code_requirements() {
        assert!(macos_autostart_needs_refresh(
            "properties = needs LWCR update | managed LWCR | has LWCR"
        ));
        assert!(macos_autostart_needs_refresh(
            "last exit reason = OS_REASON_CODESIGNING"
        ));
        assert!(!macos_autostart_needs_refresh(
            "state = running\nproperties = managed LWCR | has LWCR"
        ));
    }

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

    #[test]
    fn service_cmd_maps_known_verbs() {
        use allmystuff_service::ServiceCmd;
        assert!(matches!(
            service_cmd("install"),
            Some(ServiceCmd::Install { .. })
        ));
        assert!(matches!(service_cmd("restart"), Some(ServiceCmd::Restart)));
        assert!(matches!(
            service_cmd("uninstall"),
            Some(ServiceCmd::Uninstall)
        ));
        assert!(service_cmd("frobnicate").is_none());
    }

    #[test]
    fn windows_shell_links_match_only_final_native_extensions() {
        assert!(windows_shell_link_name("AllMyAgents.lnk"));
        assert!(windows_shell_link_name("Meeting.URL"));
        assert!(!windows_shell_link_name("notes.lnk.txt"));
        assert!(!windows_shell_link_name("meeting.url.backup"));
    }

    #[cfg(windows)]
    #[test]
    fn explorer_hidden_attributes_include_hidden_and_system_files() {
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_SYSTEM,
        };
        assert!(windows_file_is_hidden(FILE_ATTRIBUTE_HIDDEN));
        assert!(windows_file_is_hidden(FILE_ATTRIBUTE_SYSTEM));
        assert!(!windows_file_is_hidden(FILE_ATTRIBUTE_NORMAL));
    }

    #[cfg(windows)]
    #[test]
    fn local_file_paths_hide_windows_device_syntax() {
        assert_eq!(
            local_path_for_display(Path::new(r"\\?\C:\Users\Chris\Desktop")),
            r"C:\Users\Chris\Desktop"
        );
    }
}
