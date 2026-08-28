//! AllMyStuff's headless mesh **node engine**.
//!
//! This crate is the machinery that used to live inside the desktop GUI's
//! Tauri backend (`gui/src-tauri/src`). It is a *client* of a `myownmesh
//! serve` daemon over the control socket and, on top of that link, runs:
//!
//!  * **presence** — broadcasts this machine's [`NodeProfile`] and tracks
//!    peers (the graph fills with real devices);
//!  * **the route handshake** — the [`allmystuff_session::Session`] state
//!    machine, auto-accepting authorized offers (owner/fleet gated);
//!  * **every media plane** — screen + camera ([`video`], the capture
//!    sessions in [`win_capture`] / [`wayland_capture`] / [`camera_capture`]),
//!    audio ([`audio`]), input injection ([`input_inject`]), the mesh-native
//!    terminal ([`terminal`]) and file manager ([`files`]), clipboard
//!    ([`clipboard`]), and exposed-service tunnels ([`sites`]);
//!  * **ownership + the owned fleet** ([`ownership`]).
//!
//! The one thing it does *not* own is where node events surface. That seam is
//! the [`UiSink`] trait: [`mesh::Mesh::new`] takes one and the engine emits
//! through it, knowing nothing about who (if anyone) listens.
//!
//! There is **one node per machine**, and it runs in the `allmystuff-serve`
//! binary. That binary wires in a [`node_control::SocketSink`], which logs each
//! event *and* fans it out to every client connected to the node's own control
//! socket ([`node_control`]). The desktop GUI is a thin client of that socket
//! ([`node_control::NodeClient`]): it drives the node with one request per
//! command and re-emits the streamed events onto Tauri's bus, rather than
//! linking the engine and running a second `Mesh` itself. (It used to do the
//! latter, with a Tauri-backed sink — but that put two nodes under one identity
//! on an Always-On machine, and then nothing could connect.)

// The capture/inject planes live behind the default-on `host` feature: real
// on every desktop/server build, swapped for same-API no-op stubs (see
// `stubs/`) on a capture-less build — iOS, whose sandbox has no PTY, screen
// grab, input injection, or OS clipboard to offer. The stub keeps the
// module's exact public surface so `mesh.rs` (and everything else) compiles
// identically either way; only what a route *does* changes (capture routes
// report failure, inject/clipboard writes drop). Viewer planes —
// `video_decode`, the terminal output queues, opus decode — are NOT gated:
// watching other machines is the whole point of a capture-less node.
// Audio is gated separately (`audio-io`, included in `host`): it's the one
// capture plane iOS can genuinely run — cpal speaks CoreAudio there — so
// the phone builds it real while the rest stay stubs.
/// AMD AMF encode — the Radeon twin of `nvenc`, in progress (loader +
/// availability probe today; component vtables next). Runtime-loaded from
/// the Radeon driver's DLL; absent driver = absent rung, softly.
#[cfg(all(windows, feature = "host"))]
pub mod amf;
#[cfg(feature = "audio-io")]
pub mod audio;
#[cfg(not(feature = "audio-io"))]
#[path = "stubs/audio.rs"]
pub mod audio;
pub mod byte_queues;
#[cfg(feature = "host")]
pub mod camera_capture;
pub mod canvas;
pub mod cec;
pub mod child_process;
#[cfg(feature = "host")]
pub mod clipboard;
#[cfg(not(feature = "host"))]
#[path = "stubs/clipboard.rs"]
pub mod clipboard;
pub mod control_client;
/// D3D11VA (DXVA) HEVC decode — the vendor-neutral receive rung: drives
/// `ID3D11VideoDecoder` on whatever GPU is present (NVIDIA/AMD/Intel,
/// dGPU or iGPU), no codec packs, no vendor SDK. The rung that turns
/// Studio·Lossless from an NVIDIA-pair feature into any-Windows-viewer.
#[cfg(all(windows, feature = "host"))]
pub mod d3d11va;
pub mod daemon_spawn;
/// Local-only development diagnostics shared by the GUI setting and node
/// startup. This never travels over the mesh or signaling layer.
pub mod diagnostics;
pub mod drive_mount;
pub mod files;
pub mod fleetfiles;
/// Which folders of this machine other people may open, and where they
/// live. The id→path resolution a folder share turns on: the path never
/// crosses the wire, so a peer names only the minted id.
pub mod folders;
/// The GPU frame lane — D3D11 VideoProcessor colour conversion + a DXGI
/// device-manager encoder feed, keeping frames on the GPU end to end.
/// Proven by its own end-to-end test; the capture/pump integration slice
/// switches the live path onto it. Windows + host, like the MF backend it
/// pairs with.
#[cfg(all(windows, feature = "host"))]
pub mod gpu_pipeline;
/// Hardware H.264 encode via FFmpeg vendor encoders. Only built with the
/// `hwenc` feature (which pulls FFmpeg); the encoder ladder in [`video`] skips
/// it otherwise and runs software openh264.
#[cfg(feature = "hwenc")]
pub mod hwenc;
#[cfg(feature = "host")]
pub mod input_inject;
#[cfg(not(feature = "host"))]
#[path = "stubs/input_inject.rs"]
pub mod input_inject;
pub mod kvm_media;
/// The Experimental ("Labs") tier gate — one runtime choke point the Mode
/// dropdown's Experimental toggle flips, so every future field-trial
/// feature reads `labs::on(Feature::X)` and needs no new GUI control.
pub mod labs;
/// Hardware H.264 encode via Media Foundation — the GPU's own H.264 MFT
/// (NVENC/QuickSync/AMD) on Windows, with no FFmpeg toolchain. Windows-only;
/// the encoder ladder in [`video`] enumerates and frame-send-tests it, falling
/// to software openh264 when no hardware MFT produces frames. Host-gated like
/// [`video`] itself: the backend speaks the real video module's
/// [`video::EncodeOutcome`] seam, which the capture-less stub doesn't carry.
#[cfg(all(windows, feature = "host"))]
pub mod mediafoundation;
pub mod mesh;
pub mod namespace;
pub mod networks_store;
pub mod node_control;
/// NVDEC (nvcuvid) HEVC decode — the receive twin of `nvenc`, feeding the
/// native-decode lane the pictures the webview can't. Runtime-loaded from
/// the NVIDIA driver's DLLs; absent driver = absent rung, softly.
#[cfg(all(windows, feature = "host"))]
pub mod nvdec;
/// Direct NVENC (NVIDIA Video Codec SDK, runtime-loaded — no build
/// toolchain): the game-mode encoder rung with intra-refresh/GDR and
/// guaranteed in-place reconfigure, fed the GPU lane's NV12 textures.
/// Windows + host, like the GPU lane it extends; opt-in via
/// `ALLMYSTUFF_NVENC=1` until soaked.
#[cfg(all(windows, feature = "host"))]
pub mod nvenc;
pub mod operations;
/// OS performance levers for the media-plane threads (timer resolution +
/// thread priority) — Windows-real, no-op elsewhere.
pub(crate) mod os_perf;
pub mod ownership;
pub(crate) mod persist;
/// OS-level reboot of this machine — behind the gear menu's "Restart this
/// device" and the fleet's `RestartDevice` command.
pub mod reboot;
pub mod service_profiles;
pub mod shares;
pub mod sites;
pub mod storage_plan;
/// The field-test telemetry line: process/system CPU + per-engine GPU
/// utilization + VRAM every 5 s, via WDDM's vendor-neutral counters —
/// the same line on NVIDIA, AMD, and Intel boxes.
#[cfg(windows)]
pub mod telemetry;
#[cfg(feature = "host")]
pub mod terminal;
#[cfg(not(feature = "host"))]
#[path = "stubs/terminal.rs"]
pub mod terminal;
#[cfg(feature = "host")]
pub mod video;
#[cfg(not(feature = "host"))]
#[path = "stubs/video.rs"]
pub mod video;
pub mod video_decode;
/// Hardware H.264 encode via VideoToolbox — the Mac's media engine, no
/// FFmpeg toolchain; the encoder ladder in [`video`] frame-send-tests it and
/// falls back to software openh264. Host-gated like [`video`] itself: the
/// backend speaks the real video module's [`video::EncodeOutcome`] seam,
/// which the capture-less stub doesn't carry.
#[cfg(all(target_os = "macos", feature = "host"))]
pub mod videotoolbox;
#[cfg(feature = "host")]
pub mod wake;
#[cfg(windows)]
pub(crate) mod windows_fleetfiles;
// Windows screen capture (in-house DXGI). Declared on every target — the
// module is internally `cfg`-gated to a stub off Windows, exactly as it was
// when it lived in the GUI binary.
pub mod win_capture;
// Windows privilege posture + desktop following: whether a support session can
// actually reach elevated repair tooling (Event Viewer, Services, regedit) and
// the UAC secure desktop, and why not when it can't. Declared on every target
// like `win_capture` — the policy half is pure and unit-tested everywhere, and
// the Win32 half is internally `cfg`-gated to a stub off Windows.
pub mod win_privilege;
// Wayland screen capture via the ScreenCast portal — Linux only.
#[cfg(all(target_os = "linux", feature = "host"))]
pub mod wayland_capture;

/// Where the node surfaces events. Every variant the engine emits
/// (`allmystuff://session`, `…/video-ready`, `…/term-exit`, …) is a
/// front-end concern, so a headless node can drop them all; only the GUI's
/// webview actually listens.
///
/// The GUI implements this over Tauri's [`Emitter`](https://docs.rs/tauri)
/// (`app.emit`); the `allmystuff-serve` binary implements it with a logging
/// sink. Keeping it a trait object (`Arc<dyn UiSink>`) means the engine
/// links no webview and the same [`mesh::Mesh`] runs in both worlds.
pub trait UiSink: Send + Sync + 'static {
    /// Deliver one event + JSON payload to whatever front-end is attached
    /// (or nowhere). Must never block the caller — the GUI's `app.emit` is
    /// fire-and-forget, and headless sinks should be too.
    fn emit(&self, event: &str, payload: serde_json::Value);

    /// Ask an attached desktop owner to update in its own install context,
    /// then relaunch. Services and bundled nodes cannot reliably find a
    /// per-user GUI. Headless sinks have no desktop owner and ignore this.
    fn upgrade_host(&self) {}

    /// Relaunch the host process onto a freshly-applied self-update (the
    /// fleet "upgrade this machine" path). The GUI restarts its webview app;
    /// the headless node re-execs itself. Never returns.
    fn restart(&self) -> !;
}

// ---------------------------------------------------------------------------
// Runtime registry
// ---------------------------------------------------------------------------

/// The Tokio runtime the engine spawns onto, registered once at startup.
static RUNTIME: std::sync::OnceLock<tokio::runtime::Handle> = std::sync::OnceLock::new();

/// Register the runtime the engine should spawn tasks onto. [`mesh::Mesh::start`]
/// calls this with the handle of whatever runtime it's running on — Tauri's in
/// the GUI, the `allmystuff-serve` binary's own headless.
///
/// This exists because the engine fires async tasks from threads that are **not**
/// Tokio workers: screen capture (DXGI / PipeWire / AVFoundation) and audio
/// capture (cpal) each run on their own OS thread, and their callbacks need to
/// hand work back to the async world. A bare `tokio::spawn` there panics with
/// "there is no reactor running" — so the engine routes every spawn through a
/// stored [`Handle`](tokio::runtime::Handle), which is valid from any thread.
/// (This is the role `tauri::async_runtime` played while the engine still lived
/// inside the GUI.) Idempotent: the first registration wins.
pub fn set_runtime(handle: tokio::runtime::Handle) {
    let _ = RUNTIME.set(handle);
}

/// Spawn a task onto the engine's registered runtime. Unlike [`tokio::spawn`],
/// this is safe to call from any thread (a capture/audio callback included),
/// because it spawns through a stored handle rather than the ambient runtime.
///
/// Must be called after [`set_runtime`], which [`mesh::Mesh::start`] does before
/// anything captures — every spawn in the engine happens once a session is live.
pub fn spawn<F>(future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    RUNTIME
        .get()
        .expect("allmystuff-node runtime not registered — Mesh::start calls set_runtime()")
        .spawn(future)
}
