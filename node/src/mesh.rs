//! The live mesh: wires the daemon's typed channels to the
//! [`allmystuff_session::Session`] state machine and the [`AudioBridge`].
//!
//! On start it subscribes to the AllMyStuff presence / control / media
//! channels on every joined network, broadcasts this node's
//! [`NodeProfile`], and pumps inbound frames:
//!
//!  * **presence** → updates the peer set (the graph fills with real peers).
//!  * **control** → drives the route handshake; the [`Effect`]s it returns
//!    send replies and start/stop audio.
//!  * **media** → audio frames fed to the playback side of active routes.
//!
//! Everything the front-end sees comes through `allmystuff://session`
//! snapshots emitted after each change.

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::UiSink;

use allmystuff_graph::{Grant, MediaKind, NodeId, Person, PersonId, Route};
use allmystuff_protocol::{
    AppControl, ClientId, ControlMessage, NodeProfile, OwnedMember, OwnedRoster, OwnershipControl,
    Request, RoomMessage, RouteControl, ShareControl, SharedFileMeta, SiteControl, SiteService,
    TerminalSessionInfo, CHANNEL_CONTROL, CHANNEL_MEDIA, CHANNEL_PRESENCE, CHANNEL_ROOMS,
    PROTOCOL_VERSION,
};
use allmystuff_session::{
    AudioFrame, ClipboardContentKind, ClipboardEvent, ClipboardFrame, ClipboardItem, Effect,
    FileEvent, FileFrame, InputAction, InputEvent, MediaPayload, Session, SiteEvent, SiteFrame,
    TermEvent, TermFrame, VideoAssembler, VideoFrame, VideoStatusFrame, CLIPBOARD_CHUNK_BYTES,
    SITE_CHUNK_BYTES,
};

use crate::audio::{AudioBridge, CaptureSource};
use crate::clipboard::{ClipboardService, LocalClip};
use crate::control_client::{ControlClient, MediaPipe};
use crate::files::FilesPlane;
use crate::input_inject::Injector;
use crate::ownership::Ownership;
use crate::shares::Shares;
use crate::sites::{ClientMapping, SitesProxy};
use crate::terminal::{OutMsg, TerminalHost};
use crate::video::{VideoBridge, VideoMode, VideoPacket, VideoSource};
use crate::video_decode::{Au, DecodeBridge};

pub struct Mesh {
    client: Arc<ControlClient>,
    /// The media plane's dedicated daemon connection: frame chunks ride it
    /// back-to-back instead of paying a connect + round trip each.
    media_pipe: MediaPipe,
    /// Where node events surface. The GUI wires this to Tauri's event bus
    /// (`app.emit`); the headless `allmystuff serve` binary uses a logging
    /// sink — the events are all front-end concerns, so a node with no UI
    /// simply drops them. See [`crate::UiSink`].
    sink: Arc<dyn UiSink>,
    audio: Arc<AudioBridge>,
    /// Screen + camera capture for the display/video routes this machine
    /// sources (the far end of a console session looking at us, a room
    /// member watching our camera).
    video: Arc<VideoBridge>,
    /// Native H.264 decode for inbound display routes whose console window
    /// asked for ready-to-paint frames (no WebCodecs in its webview, or its
    /// decoder stalled out).
    video_decode: Arc<DecodeBridge>,
    /// Keyboard/mouse injection for input routes that sink here — gated on
    /// the sender being our owner or a fleet member.
    injector: Injector,
    /// Mesh-native terminal sessions: PTYs this machine hosts for terminal
    /// routes sourcing here (gated like input injection), and the output
    /// buffers terminal windows drain for routes sinking here.
    terminal: TerminalHost,
    /// Sequence for viewer-side outbound terminal frames (keystrokes,
    /// resizes — one stream per app run, like `input_seq`).
    term_seq: AtomicU64,
    /// Viewer-route ids that already have a live host/loopback output pump.
    /// Exactly one pump per route: a duplicate `StartMedia` — e.g. the offer
    /// delivered on more than one shared network — must not spawn a second
    /// pump onto the same route, which would fan the shell's output out twice
    /// (the cause of doubled/tripled terminals on a multi-network peer).
    term_pumps: Mutex<std::collections::HashSet<String>>,
    /// Highest terminal-frame `seq` already taken per route, each direction:
    /// `term_rx_seq` is output the *viewer* takes from the host;
    /// `term_in_seq` is input the *host* takes from the viewer. Both sending
    /// sides number a route's frames strictly increasing, so a seq we've
    /// already seen is a duplicate delivery (the same send arriving on several
    /// shared networks) — dropped, not re-applied. Without the input one, a
    /// keystroke redelivered N times is written to the PTY N times and the
    /// shell echoes `aaaa`.
    term_rx_seq: Mutex<HashMap<String, u64>>,
    term_in_seq: Mutex<HashMap<String, u64>>,
    /// Mesh-native file sessions: filesystem ops this machine hosts for
    /// files routes sourcing here (gated like the terminal), and the
    /// response buffers files windows drain for routes sinking here.
    files: FilesPlane,
    /// Sequence for outbound file frames (requests viewer-side, response
    /// streams host-side — one stream per app run, like `term_seq`).
    file_seq: AtomicU64,
    /// Mesh-native sites: this machine's exposed-service allow-list + the
    /// live reverse-proxy connections (client mappings sinking here, host
    /// tunnels sourcing here). See [`SitesProxy`].
    sites: SitesProxy,
    /// Sequence for outbound site frames (one stream per app run, like the
    /// other media-plane sequences).
    site_seq: AtomicU64,
    /// Viewer-side download sinks: a `(route, req)` whose `Chunk`s should
    /// stream straight to a local file (the Downloads folder) instead of
    /// the window's queue — registered by `file_download` *before* the
    /// Read request goes out, so the first chunk can't race it.
    downloads: Mutex<HashMap<(String, u64), DownloadSink>>,
    /// Host-side **Shared Files** registry: the files this machine has
    /// offered into rooms, keyed by the opaque token the uploader handed
    /// out. A `:shared` route can only `Fetch` by token (never browse a
    /// path), and a fetch is served only when the requester's pubkey is in
    /// the token's `allowed` set (the room's members) — so a call's shared
    /// area never becomes a way to read the disk. Bytes flow straight to
    /// the downloader; the room host only ever carries the *list*.
    shared: Mutex<HashMap<String, SharedReg>>,
    state: Mutex<State>,
    /// This device's persisted ownership record — who owns it and whether
    /// it's currently offering itself for adoption (claim mode).
    ownership: Arc<Ownership>,
    /// Canonical pubkeys authorised to control this device — the fleet's
    /// **closed-network signed roster**, cached from the daemon (`RosterList`
    /// for `ownership.fleet_network_id()`). [`Mesh::sender_may_control`] trusts
    /// THIS alone: membership is established by the owner founding a genuinely
    /// closed network (founder self-election) and admitting members into the
    /// signed roster, so no unauthenticated gossip can grant control — closing
    /// the fleet-conscription takeover (AMS-01). Refreshed on ownership changes
    /// and on a periodic tick.
    fleet_authorized: Mutex<std::collections::HashSet<String>>,
    /// Durable share relationships — who I share with and the grants in each
    /// direction. Node-owned (enforcement lives here), persisted beside the
    /// ownership record, and projected into [`Mesh::snapshot`] so the GUI
    /// renders a peer as *shared* with its grants across a restart.
    shares: Arc<Shares>,
    /// Outbound audio: capture callbacks push `(peer, frame)`; a forwarder
    /// task sends them on the media channel. Bounded like video: a stalled
    /// link sheds buffers (a brief skip) instead of queueing a backlog the
    /// listener then hears seconds late.
    audio_out: mpsc::Sender<AudioOut>,
    /// Outbound video, deliberately *bounded*: when the link can't keep up
    /// the capture side drops frames instead of queueing stale ones (an
    /// MJPEG drop costs freshness only; an H.264 drop is healed by the
    /// next forced IDR).
    video_out: mpsc::Sender<(String, String, VideoPacket)>,
    /// The matching receivers, parked by [`Mesh::new`] and drained by the
    /// forwarder tasks [`Mesh::start`] spawns. They live here rather than
    /// being spawned in `new` because the GUI builds the `Mesh` in a
    /// *synchronous* Tauri `setup` (no ambient Tokio runtime to spawn on);
    /// `start` is the first point guaranteed an async context, and on the
    /// same runtime everything else runs on.
    audio_rx: Mutex<Option<mpsc::Receiver<AudioOut>>>,
    video_rx: Mutex<Option<mpsc::Receiver<(String, String, VideoPacket)>>>,
    /// Sequence for outbound input events (one stream per app run).
    input_seq: AtomicU64,
    /// Sequence for outbound clipboard frames (one stream per app run, like
    /// `input_seq` — clipboard rides alongside control).
    clipboard_seq: AtomicU64,
    /// Transfer ids for outbound clipboard image/file pastes — scopes a
    /// transfer's chunks, separate from the per-frame `clipboard_seq`.
    clipboard_transfer: AtomicU64,
    /// The OS clipboard on its own thread — reads on paste, writes on
    /// receipt (see [`crate::clipboard`]).
    clipboard: ClipboardService,
    /// Inbound clipboard transfers being reassembled, keyed by (route,
    /// transfer id). Image bytes accumulate in memory; file bytes stream to
    /// a per-transfer staging dir.
    clip_inbound: Mutex<HashMap<(String, u64), ClipInbound>>,
    /// When we last sent a clipboard [`Pull`](ClipboardEvent::Pull) per route
    /// — the gate that lets the remote's reply land on *our* clipboard. Only a
    /// reply that arrives within [`CLIPBOARD_PULL_WINDOW`] of our own pull is
    /// accepted, so a misbehaving peer can't clobber our clipboard unasked.
    clip_pull_at: Mutex<HashMap<String, std::time::Instant>>,
    /// This app run's random presence boot id — how peers detect that we
    /// (re)started and answer with their state (see `NodeProfile::boot`).
    boot_id: u64,
    /// Reassembles chunked inbound video frames (a frame bigger than the
    /// data channel's ~64 KiB message ceiling arrives in pieces).
    video_in: Mutex<VideoAssembler>,
    /// Per-route queues of ready-to-ship packets (28-byte header +
    /// payload) for the console windows watching inbound video. The
    /// webview *pulls* these (`video_poll`, one drain per display
    /// refresh): a pull that fails costs one tick, where the previous
    /// push channel's ordered delivery meant one lost message silently
    /// froze the stream forever while the backend kept counting frames.
    video_watchers: Mutex<HashMap<String, VideoWatcher>>,
    /// Whether the local daemon speaks the video track lane (`video_*`
    /// ops, myownmesh ≥ 0.2.1). Probed at session start; while false the
    /// app neither offers nor picks H.264 — screen shares ride MJPEG and
    /// a single loud log says why. This is what keeps a stale daemon a
    /// slow stream instead of a black one.
    daemon_video: std::sync::atomic::AtomicBool,
    /// Inbound per-route counters (frames, bytes), logged every few
    /// seconds — the receive half of the dial-in line the sender's
    /// `StreamStats` provides.
    video_in_stats: Mutex<HashMap<String, VideoInStats>>,
    /// Last emission per inbound-video diagnostic key — the rate limit
    /// behind [`Self::diag_ok`], so a dead stream explains itself once per
    /// [`WARN_EVERY`] instead of at frame rate.
    video_diag_last: Mutex<HashMap<String, std::time::Instant>>,
    /// When each route last asked its sender for a clean decode entry —
    /// decode errors arrive at frame rate; the asks must not.
    refresh_asks: Mutex<HashMap<String, std::time::Instant>>,
    /// Per-route Opus decoders for inbound lane audio (stateful across
    /// frames; dropped with the route).
    audio_decoders: Mutex<HashMap<String, opus::Decoder>>,
    /// Whether the local daemon speaks the audio track lane (`audio_*`
    /// ops, myownmesh ≥ 0.2.4) — the audio twin of `daemon_video`.
    /// While false, audio rides PCM frames over the media channel.
    daemon_audio: std::sync::atomic::AtomicBool,
    /// How many media lanes the local daemon provisions per peer (from
    /// Status `media_lanes`); 1 means a pre-pool daemon.
    daemon_lanes: std::sync::atomic::AtomicU8,
}

/// One captured-audio packet headed for the forwarder, in whichever
/// shape its route negotiated.
enum AudioOut {
    /// A PCM frame for `CHANNEL_MEDIA` — the floor every peer speaks.
    Channel(String, AudioFrame),
    /// One encoded Opus frame for the daemon's audio track lane.
    Lane {
        peer: String,
        route: String,
        data: Vec<u8>,
    },
}

/// One console window's claim on a route's inbound packets: the queue it
/// drains plus the token that claim was made with — `video_unwatch`
/// removes the queue only when the token still matches, so a stale
/// unwatch (a torn-down watcher racing the next one over async IPC)
/// can't delete its successor's queue.
struct VideoWatcher {
    token: u64,
    /// Whether this window asked the backend to decode H.264 for it
    /// (raw RGBA frames out) instead of passing access units through.
    decode: bool,
    queue: std::collections::VecDeque<Vec<u8>>,
}

/// One registered "save this download to disk" sink: the open file the
/// chunks stream into, where it lives, and progress accounting for the
/// `allmystuff://file-progress` events.
struct DownloadSink {
    file: std::fs::File,
    path: std::path::PathBuf,
    written: u64,
    last_progress: std::time::Instant,
}

/// One file offered into a room's Shared Files area: the absolute path on
/// this disk and the pubkeys allowed to fetch it (the room's members, as
/// the uploader stated them). The token that keys it in `Mesh::shared` is
/// what travels — never this path.
struct SharedReg {
    path: std::path::PathBuf,
    allowed: std::collections::HashSet<String>,
}

/// Receive-side counters for one route's stream.
struct VideoInStats {
    since: std::time::Instant,
    frames: u32,
    bytes: u64,
    label: &'static str,
}

impl VideoInStats {
    fn new(label: &'static str) -> Self {
        VideoInStats {
            since: std::time::Instant::now(),
            frames: 0,
            bytes: 0,
            label,
        }
    }
}

/// Raw JPEG bytes per video chunk: after base64 (+33%) and the JSON
/// envelope, a chunk message stays comfortably under the data channel's
/// ~64 KiB ceiling (the WebRTC SCTP max message size).
const MAX_JPEG_CHUNK_BYTES: usize = 40 * 1024;

/// Raw PTY bytes per terminal Data frame — same ceiling arithmetic as the
/// video chunks, sized small so a `cat bigfile` interleaves with
/// keystrokes instead of wedging the channel behind one giant message.
const MAX_TERM_DATA_BYTES: usize = 16 * 1024;

/// A terminal host whose sends keep failing this long (viewer offline,
/// network gone) kills the shell and tears the route down — nothing else
/// reaps a session whose peer silently vanished.
const TERM_SEND_PATIENCE: std::time::Duration = std::time::Duration::from_secs(60);

/// Initial PTY size for a freshly opened terminal session — the viewer's
/// first `Resize` reconciles the shared PTY to its real emulator size
/// moments later (and an attach to an existing session keeps that session's
/// reconciled size). A sane 80×24 beats a 0×0 PTY in the gap.
const TERM_INIT_COLS: u16 = 80;
const TERM_INIT_ROWS: u16 = 24;

/// Media-plane send failures repeat at frame rate; warn at most this often.
const WARN_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

struct State {
    session: Option<Session>,
    /// Primary network — the fallback for route control/media when we don't
    /// yet know which network a peer is on.
    network: Option<String>,
    /// Every joined network. Presence is broadcast on all of them so peers
    /// find each other regardless of which network the daemon lists first.
    networks: Vec<String>,
    /// Which network each peer was last seen on (canonical pubkey → network
    /// config_id). You can be on several networks at once and a given peer may
    /// only share one of them, so control/media must be addressed to the
    /// network that peer actually lives on — not a single "primary" mesh.
    peer_networks: HashMap<String, String>,
    /// App features each peer last advertised (canonical pubkey → feature
    /// list from its presence profile). Read to decide whether a peer can
    /// ride the media-lane pool — `FEATURE_MEDIA_LANES` present means both
    /// ends ship the lane-pool daemon and can split streams across lanes.
    peer_features: HashMap<String, Vec<String>>,
    /// Last presence boot id seen per peer (canonical pubkey). A boot id we
    /// haven't recorded means the peer just (re)started and missed our
    /// adverts — we answer with our state directly. This is what lets
    /// gossip be event-driven instead of a heartbeat.
    peer_boots: HashMap<String, u64>,
    client_id: Option<ClientId>,
    profile: Option<NodeProfile>,
}

impl Mesh {
    pub fn new(client: Arc<ControlClient>, sink: Arc<dyn UiSink>) -> Arc<Self> {
        // Shallow queues both: at most a few frames in flight, so a slow
        // link sheds load by dropping captures rather than growing latency.
        // Audio's 8 buffers are ~160 ms of slack.
        let (audio_out, audio_rx) = mpsc::channel::<AudioOut>(8);
        let (video_out, video_rx) = mpsc::channel::<(String, String, VideoPacket)>(4);
        Arc::new(Mesh {
            client: client.clone(),
            media_pipe: MediaPipe::new(client.clone()),
            sink,
            audio: Arc::new(AudioBridge::new()),
            video: Arc::new(VideoBridge::new()),
            video_decode: Arc::new(DecodeBridge::new()),
            injector: Injector::new(),
            terminal: TerminalHost::new(),
            term_seq: AtomicU64::new(0),
            term_pumps: Mutex::new(std::collections::HashSet::new()),
            term_rx_seq: Mutex::new(HashMap::new()),
            term_in_seq: Mutex::new(HashMap::new()),
            files: FilesPlane::new(),
            file_seq: AtomicU64::new(0),
            sites: SitesProxy::load(),
            site_seq: AtomicU64::new(0),
            downloads: Mutex::new(HashMap::new()),
            shared: Mutex::new(HashMap::new()),
            state: Mutex::new(State {
                session: None,
                network: None,
                networks: Vec::new(),
                peer_networks: HashMap::new(),
                peer_features: HashMap::new(),
                peer_boots: HashMap::new(),
                client_id: None,
                profile: None,
            }),
            ownership: Arc::new(Ownership::load()),
            fleet_authorized: Mutex::new(std::collections::HashSet::new()),
            shares: Arc::new(Shares::load()),
            audio_out,
            video_out,
            audio_rx: Mutex::new(Some(audio_rx)),
            video_rx: Mutex::new(Some(video_rx)),
            input_seq: AtomicU64::new(0),
            clipboard_seq: AtomicU64::new(0),
            clipboard_transfer: AtomicU64::new(0),
            clipboard: ClipboardService::spawn(),
            clip_inbound: Mutex::new(HashMap::new()),
            clip_pull_at: Mutex::new(HashMap::new()),
            boot_id: fresh_boot_id(),
            video_in: Mutex::new(VideoAssembler::new()),
            video_watchers: Mutex::new(HashMap::new()),
            daemon_video: std::sync::atomic::AtomicBool::new(false),
            video_in_stats: Mutex::new(HashMap::new()),
            video_diag_last: Mutex::new(HashMap::new()),
            refresh_asks: Mutex::new(HashMap::new()),
            audio_decoders: Mutex::new(HashMap::new()),
            daemon_audio: std::sync::atomic::AtomicBool::new(false),
            daemon_lanes: std::sync::atomic::AtomicU8::new(1),
        })
    }

    /// Spawn the media forwarders that drain captured frames out to peers on
    /// the media channel, both bounded (see the field docs). Send failures are
    /// *surfaced* (rate-limited): a silently-dying media plane is exactly the
    /// "connected but nothing arrives" mystery.
    ///
    /// Called from [`Mesh::start`] rather than [`Mesh::new`] so the tasks land
    /// on the runtime `start` runs on — `new` is built in the GUI's sync Tauri
    /// `setup`, where `tokio::spawn` would panic with "no reactor running".
    /// Idempotent: the receivers are taken once, so a second call is a no-op.
    fn spawn_media_forwarders(self: &Arc<Self>) {
        if let Some(mut audio_rx) = self.audio_rx.lock().take() {
            let mesh = self.clone();
            crate::spawn(async move {
                let mut last_warn = std::time::Instant::now() - WARN_EVERY;
                while let Some(out) = audio_rx.recv().await {
                    let (peer, result) = match out {
                        AudioOut::Channel(peer, frame) => {
                            let Ok(payload) = serde_json::to_value(&frame) else {
                                continue;
                            };
                            let r = mesh.send_media_value(&peer, payload).await;
                            (peer, r)
                        }
                        AudioOut::Lane { peer, route, data } => {
                            let lane = mesh.audio_lane(&route, &peer, true).unwrap_or(0);
                            let r = mesh.send_audio_track(&peer, lane, data).await;
                            (peer, r)
                        }
                    };
                    if let Err(e) = result {
                        if last_warn.elapsed() >= WARN_EVERY {
                            last_warn = std::time::Instant::now();
                            tracing::warn!("audio frame to {} failed: {e}", short_id(&peer));
                        }
                    }
                }
            });
        }
        if let Some(mut video_rx) = self.video_rx.lock().take() {
            let mesh = self.clone();
            crate::spawn(async move {
                let mut last_warn = std::time::Instant::now() - WARN_EVERY;
                while let Some((peer, route_id, packet)) = video_rx.recv().await {
                    let outcome = match packet {
                        // An MJPEG frame above the data channel's message
                        // ceiling travels as several chunks sharing a seq.
                        VideoPacket::Jpeg(frame) => {
                            let mut result = Ok(());
                            for chunk in frame.into_chunks(MAX_JPEG_CHUNK_BYTES) {
                                let Ok(payload) = serde_json::to_value(&chunk) else {
                                    continue;
                                };
                                if let Err(e) = mesh.send_media_value(&peer, payload).await {
                                    result = Err(e);
                                    break; // rest of this frame is pointless
                                }
                            }
                            result
                        }
                        // An H.264 access unit rides the mesh's RTP track
                        // lane — no chunking (RTP packetizes), no ceiling.
                        VideoPacket::H264 { data, duration_us } => {
                            let lane = mesh.video_lane(&route_id, &peer, true).unwrap_or(0);
                            mesh.send_video_track(&peer, lane, data, duration_us).await
                        }
                    };
                    if let Err(e) = outcome {
                        if last_warn.elapsed() >= WARN_EVERY {
                            last_warn = std::time::Instant::now();
                            tracing::warn!("video to {} failed: {e}", short_id(&peer));
                        }
                    }
                }
            });
        }
    }

    /// Send one media-channel payload to `peer` (canonicalised to the bare
    /// pubkey the daemon's peer set is keyed by) down the pipelined media
    /// pipe. `Ok` means the daemon has the bytes; its verdict (peer gone,
    /// message too large) still reaches a log — the pipe's response drain
    /// warns on refusals instead of this path stalling a round trip per
    /// chunk to hear them.
    async fn send_media_value(&self, peer: &str, payload: Value) -> Result<(), String> {
        let Some(network) = self.network_for_peer(peer) else {
            return Err("no shared network".into());
        };
        self.media_pipe
            .send(&Request::ChannelSendTo {
                network,
                channel: CHANNEL_MEDIA.to_string(),
                peer: pubkey_part(peer).to_string(),
                payload,
            })
            .await
            .map_err(|e| e.to_string())
    }

    /// Send one H.264 access unit to `peer` over the daemon's video
    /// track lane (base64 on the control socket, RTP on the wire).
    async fn send_video_track(
        &self,
        peer: &str,
        lane: u8,
        data: Vec<u8>,
        duration_us: u64,
    ) -> Result<(), String> {
        use base64::Engine as _;
        let Some(network) = self.network_for_peer(peer) else {
            return Err("no shared network".into());
        };
        self.media_pipe
            .send(&Request::VideoSend {
                network,
                peer: pubkey_part(peer).to_string(),
                stream: lane,
                duration_us,
                data: base64::engine::general_purpose::STANDARD.encode(&data),
            })
            .await
            .map_err(|e| e.to_string())
    }

    /// Send one encoded Opus frame to `peer` over the daemon's audio
    /// track lane (base64 on the control socket, RTP on the wire).
    async fn send_audio_track(&self, peer: &str, lane: u8, data: Vec<u8>) -> Result<(), String> {
        use base64::Engine as _;
        let Some(network) = self.network_for_peer(peer) else {
            return Err("no shared network".into());
        };
        self.media_pipe
            .send(&Request::AudioSend {
                network,
                peer: pubkey_part(peer).to_string(),
                stream: lane,
                duration_us: crate::audio::OPUS_FRAME_US,
                data: base64::engine::general_purpose::STANDARD.encode(&data),
            })
            .await
            .map_err(|e| e.to_string())
    }

    /// The network to reach `peer` on: the one we last saw them advertise on,
    /// falling back to the primary. This is what lets a connection cross to a
    /// peer that only shares a secondary network with us.
    fn network_for_peer(&self, peer: &str) -> Option<String> {
        let st = self.state.lock();
        st.peer_networks
            .get(pubkey_part(peer))
            .cloned()
            .or_else(|| st.network.clone())
    }

    /// This node's mesh id once known (the daemon device id), else `None`.
    pub fn local_node_id(&self) -> Option<String> {
        self.state
            .lock()
            .session
            .as_ref()
            .map(|s| s.me().to_string())
    }

    /// This node's mesh id, resolved even before the live session starts: the
    /// session id once `start()` has run, else the daemon identity's device id
    /// (available as soon as the control socket is up). So a scan at launch
    /// already carries the real id and the local node never lingers under the
    /// `"this"` placeholder (which is what made this machine briefly show as a
    /// bare "not on AllMyStuff" twin). `None` only when the daemon is
    /// unreachable.
    pub async fn resolve_local_id(&self) -> Option<String> {
        if let Some(id) = self.local_node_id() {
            return Some(id);
        }
        self.fetch_identity().await
    }

    /// Bring the session online: identify, pick a network, subscribe, and
    /// start pumping events. Safe to call once the daemon socket is up.
    pub async fn start(self: Arc<Self>) {
        // Register the runtime we're on so the engine can spawn from any
        // thread — capture/audio callbacks run on their own OS threads, where
        // a bare `tokio::spawn` panics ("no reactor running"). All engine
        // spawns go through `crate::spawn`, which uses this handle. Set first,
        // before anything (the forwarders below) spawns.
        crate::set_runtime(tokio::runtime::Handle::current());

        // Spawn the media forwarders now that we're on a runtime (see
        // `spawn_media_forwarders` — `new` runs in the GUI's sync setup).
        self.spawn_media_forwarders();

        let (tx, mut rx) = mpsc::channel::<Value>(512);
        let client_id = match self.client.subscribe_events(tx).await {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!("mesh: event subscribe failed: {e}");
                self.emit_status("disconnected", Some(&e.to_string()));
                return;
            }
        };

        // Identity → our node id + presence profile. The label is the
        // user's optional override; `build_profile` falls back to the
        // hostname when it's unset.
        let me = self
            .fetch_identity()
            .await
            .unwrap_or_else(|| NodeId::this().to_string());
        let label = self.fetch_identity_label().await;
        let profile = self.build_profile(&me, label);
        // Every joined network; route control/media operate on the primary.
        let networks = self.fetch_networks().await;
        let primary = networks.first().cloned();

        {
            let mut st = self.state.lock();
            st.client_id = Some(client_id);
            st.session = Some(Session::new(me.clone()));
            st.profile = Some(profile.clone());
            st.network = primary.clone();
            st.networks = networks.clone();
        }

        if networks.is_empty() {
            // Still run the claim-status check (it sanitizes stale fleet
            // residue and refreshes the UI); the broadcasts inside are
            // no-ops with no networks to send on.
            self.ownership_check(None).await;
            self.emit_status("no_network", None);
        } else {
            // Every AllMyStuff channel on *every* network. Presence + the
            // owned-fleet roster so two machines discover each other (and
            // converge their fleet) no matter which network the daemon lists
            // first — and control + media too, because point-to-point traffic
            // is addressed to whichever network *we* last saw the peer on,
            // which need not be the peer's first-listed one. With these on
            // the primary only, a claim or route offer arriving on a shared
            // secondary network had no subscriber on the receiving side and
            // the daemon silently dropped it.
            self.subscribe_channels(client_id, &networks).await;
            // App-load trigger of the claim-status check: sanitize stale
            // fleet residue, then assert presence + roster to everyone.
            self.ownership_check(None).await;
            self.emit_status("live", None);
        }

        // No periodic re-broadcast: gossip is event-driven. Late joiners are
        // covered twice over — the daemon's "peer approved" event triggers a
        // targeted ownership check at them, and a presence advert carrying a
        // boot id we haven't recorded (their app just started while the
        // daemon link stayed up) gets answered with our state directly. The
        // mesh carries traffic when something *happens*, not on a heartbeat.
        //
        // Devices, though, change under a running app — a monitor wakes (or
        // deep-sleeps and *detaches*: DP monitors drop off the desktop), a
        // mic gets plugged in — and the profile peers hold was scanned once
        // at start. The watcher below re-scans on a slow cadence and counts
        // as "something happened" only when the picture actually changed.
        self.spawn_inventory_watch();

        // Event loop.
        let mesh = self.clone();
        crate::spawn(async move {
            while let Some(value) = rx.recv().await {
                mesh.handle_value(value).await;
            }
            mesh.emit_status("disconnected", None);
        });
    }

    /// Re-scan this machine's inventory every [`INVENTORY_RESCAN`] and
    /// refresh the live presence profile when the device picture changed,
    /// so a display that woke (or detached), a camera that appeared, or a
    /// changed default reaches the graph — local drawer and peers both —
    /// without an app restart. The scan is cheap by design ("cheap enough
    /// to call on a button press"), and steady state broadcasts nothing.
    fn spawn_inventory_watch(self: &Arc<Self>) {
        const INVENTORY_RESCAN: std::time::Duration = std::time::Duration::from_secs(10);
        let mesh = Arc::downgrade(self);
        crate::spawn(async move {
            loop {
                tokio::time::sleep(INVENTORY_RESCAN).await;
                let Some(mesh) = mesh.upgrade() else {
                    return;
                };
                let Some(node) = mesh.state.lock().profile.as_ref().map(|p| p.node.clone()) else {
                    continue; // live session not up yet
                };
                let scanned = tokio::task::spawn_blocking(move || {
                    let inv = allmystuff_inventory::scan();
                    (
                        allmystuff_bridge::node_summary(&inv),
                        allmystuff_bridge::capabilities_with_screens(
                            &inv,
                            &node,
                            &crate::video::extra_screens(),
                        ),
                    )
                })
                .await;
                let Ok((summary, capabilities)) = scanned else {
                    continue;
                };
                let changed = {
                    let mut st = mesh.state.lock();
                    let Some(p) = st.profile.as_mut() else {
                        continue;
                    };
                    let fresh = profile_fingerprint(&summary, &capabilities);
                    if profile_fingerprint(&p.summary, &p.capabilities) == fresh {
                        false
                    } else {
                        p.summary = summary;
                        p.capabilities = capabilities;
                        true
                    }
                };
                if changed {
                    tracing::info!("device picture changed on rescan — re-broadcasting presence");
                    mesh.broadcast_presence().await;
                    // Keep the peer-list copy of the summary fresh too, so peers
                    // that read it from the capability matrix (not the presence
                    // advert) see the new stats.
                    mesh.advertise_capabilities().await;
                    mesh.emit_snapshot();
                }
            }
        });
    }

    async fn fetch_identity(&self) -> Option<String> {
        let resp = self.client.request(&Request::IdentityShow).await.ok()?;
        resp.data?
            .get("device_id")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }

    /// The user's optional display-name override from the daemon identity.
    /// `None` (or empty) means "use the hostname".
    async fn fetch_identity_label(&self) -> Option<String> {
        let resp = self.client.request(&Request::IdentityShow).await.ok()?;
        resp.data?
            .get("label")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty())
    }

    /// Update this node's display label (the identity override) on the live
    /// presence profile and re-broadcast so peers pick it up. An empty label
    /// resets the display to the machine hostname.
    pub async fn set_label(self: &Arc<Self>, label: String) {
        {
            let mut st = self.state.lock();
            if let Some(p) = st.profile.as_mut() {
                p.label = if label.trim().is_empty() {
                    p.hostname.clone()
                } else {
                    label
                };
            }
        }
        self.broadcast_presence().await;
    }

    /// Recompute this node's advertised `sites` from a fresh scan + the
    /// current exposed set, then re-broadcast presence — so a change to what
    /// the owner exposes reaches peers' Sites tabs promptly. User-triggered
    /// and rare, so the scan here is well off any hot path.
    async fn restamp_profile(self: &Arc<Self>) {
        // Scan off the async runtime (lsof on macOS, /proc walks on Linux).
        let mesh = self.clone();
        let sites = tokio::task::spawn_blocking(move || {
            let inv = allmystuff_inventory::scan();
            allmystuff_bridge::sites::sites_from_inventory(&inv, &mesh.sites.exposed_map())
        })
        .await
        .unwrap_or_default();
        let count = sites.len();
        {
            let mut st = self.state.lock();
            if let Some(p) = st.profile.as_mut() {
                p.sites = sites;
            }
        }
        tracing::info!("re-advertising {count} exposed site(s) to peers");
        self.reassert_presence().await;
        // Our own UI (and any console window) reflects the change at once.
        self.emit_snapshot();
    }

    /// Push this node's presence out so a change reaches every connected
    /// peer: the broadcast to all, *and* a targeted send to each peer the
    /// session already knows. The targeted half is the belt-and-suspenders —
    /// a `ChannelSendAll` can miss an already-connected peer mid-session,
    /// where a `ChannelSendTo` per peer lands (the same path that answers a
    /// peer that just restarted).
    async fn reassert_presence(self: &Arc<Self>) {
        self.broadcast_presence().await;
        let peers: Vec<String> = {
            let st = self.state.lock();
            st.session
                .as_ref()
                .map(|s| s.peers().map(|p| p.node.to_string()).collect())
                .unwrap_or_default()
        };
        for peer in peers {
            self.send_presence_to(&peer).await;
        }
    }

    /// All joined networks' config ids. The daemon wraps the list as
    /// `{ "networks": [...] }`, so we read that field (an earlier version
    /// called `as_array()` on the wrapper and always got nothing — which left
    /// presence un-subscribed and peers unable to see each other).
    async fn fetch_networks(&self) -> Vec<String> {
        let Some(resp) = self.client.request(&Request::NetworksList).await.ok() else {
            return Vec::new();
        };
        resp.data
            .as_ref()
            .and_then(|d| d.get("networks"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|n| {
                        n.get("config_id")
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn build_profile(&self, me: &str, label_override: Option<String>) -> NodeProfile {
        let inv = allmystuff_inventory::scan();
        let node = NodeId::from(me);
        let hostname = inv.host.hostname.clone();
        // Display name = override if the user set one, else the hostname.
        let label = label_override
            .filter(|l| !l.trim().is_empty())
            .unwrap_or_else(|| hostname.clone());
        NodeProfile {
            protocol: PROTOCOL_VERSION,
            node: node.clone(),
            label,
            hostname,
            summary: allmystuff_bridge::node_summary(&inv),
            capabilities: allmystuff_bridge::capabilities_with_screens(
                &inv,
                &node,
                &crate::video::extra_screens(),
            ),
            // Tell peers who owns this device and whether it's up for
            // adoption, so they can't silently grab a box that's already
            // spoken for (or one that was never put into claim mode).
            owner: self.ownership.owner().map(NodeId::from),
            claimable: self.ownership.claimable(),
            boot: self.boot_id,
            // This build can host mesh-native terminals on every OS the
            // app ships for (openpty / ConPTY) — advertise it so peers
            // know to offer one. Runtime spawn failures still degrade
            // in-band (the viewer sees the error in its terminal). Same
            // for file sessions: plain std::fs everywhere we ship.
            // …and it speaks the virtual-rooms plane (invites, join/leave,
            // chat on CHANNEL_ROOMS), so room UIs can badge members that
            // can't hear them. Camera streaming likewise rides every OS
            // (V4L2 / AVFoundation / Media Foundation); a camera that
            // won't open at route time degrades in-band too (`vstat`).
            features: {
                let mut f = vec![
                    allmystuff_protocol::FEATURE_TERMINAL.to_string(),
                    allmystuff_protocol::FEATURE_FILES.to_string(),
                    allmystuff_protocol::FEATURE_ROOMS.to_string(),
                    allmystuff_protocol::FEATURE_CAMERA.to_string(),
                    allmystuff_protocol::FEATURE_SITES.to_string(),
                ];
                if self.daemon_lanes.load(Ordering::SeqCst) > 1 {
                    f.push(allmystuff_protocol::FEATURE_MEDIA_LANES.to_string());
                }
                f
            },
            // Only the services the owner opted to expose (the exposed set is
            // the host's allow-list); a scan that found a dozen listeners
            // advertises only those, each under its chosen name. Empty until
            // the user exposes one.
            sites: allmystuff_bridge::sites::sites_from_inventory(&inv, &self.sites.exposed_map()),
            // The build this process is running, so a fleet peer can tell
            // when this machine is behind the channel's latest release and
            // offer to upgrade it. It's the running binary's own version: a
            // staged update only becomes our reported version once we restart
            // onto it (which an `Upgrade` triggers), so this stays honest.
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// Advertise an AllMyStuff marker (plus this build's features and version)
    /// on the **mesh** capability matrix, so every peer learns through the
    /// reliable handshake + peer-list that this is an app node — not a bare
    /// `myownmesh` daemon — independent of the bespoke presence broadcast. The
    /// receiver flips a peer to "on AllMyStuff" off its polled peer view, so a
    /// dropped presence advert no longer leaves a connected peer mesh-only.
    /// Idempotent: `CapabilitiesSet` replaces the advertised matrix, so
    /// re-running it on each network sync is cheap.
    async fn advertise_capabilities(&self) {
        let (networks, profile) = {
            let st = self.state.lock();
            (st.networks.clone(), st.profile.clone())
        };
        let mut tags = vec![allmystuff_protocol::CAP_TAG_ALLMYSTUFF.to_string()];
        if let Some(p) = &profile {
            tags.extend(p.features.iter().cloned());
        }
        let capabilities = json!({
            "tags": tags,
            "app_version": env!("CARGO_PKG_VERSION"),
            // The daemon's `CapabilityAdvert` is a typed struct — only `tags`,
            // `app_version`, `max_connections`, and a freeform `extra` survive
            // its (de)serialization. Anything app-specific MUST ride `extra`,
            // or serde drops it at the control boundary (which silently sank an
            // earlier attempt to carry these at the top level). So nest the
            // embedder data under `extra`:
            //  - summary: the device stats (OS / CPU / RAM / device count), so a
            //    peer whose bespoke presence frame was missed still shows them.
            //  - endpoints: the wireable control / audio / video / display sinks
            //    & sources rooms and remote-control resolve a route through.
            //    These used to ride *only* the flaky presence advert, so a missed
            //    frame left a peer showing its buttons but advertising no
            //    endpoint — "no audio/control/video path to that machine". The
            //    polled peer list is reliable, so a path resolves regardless.
            "extra": {
                "summary": profile.as_ref().map(|p| &p.summary),
                "endpoints": profile.as_ref().map(|p| &p.capabilities),
            },
        });
        for network in networks {
            let _ = self
                .client
                .request(&Request::CapabilitiesSet {
                    network,
                    capabilities: capabilities.clone(),
                })
                .await;
        }
    }

    async fn broadcast_presence(&self) {
        let (networks, profile) = {
            let st = self.state.lock();
            (st.networks.clone(), st.profile.clone())
        };
        let Some(profile) = profile else { return };
        let Ok(payload) = serde_json::to_value(&profile) else {
            return;
        };
        for network in networks {
            let _ = self
                .client
                .request(&Request::ChannelSendAll {
                    network,
                    channel: CHANNEL_PRESENCE.to_string(),
                    payload: payload.clone(),
                })
                .await;
        }
    }

    async fn handle_value(self: &Arc<Self>, value: Value) {
        let Some(kind) = value.get("kind").and_then(|v| v.as_str()) else {
            return;
        };
        match kind {
            "channel_inbound" => {
                let channel = value.get("channel").and_then(|v| v.as_str()).unwrap_or("");
                let from = value
                    .get("from")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                // The network this frame arrived on — so we learn which network
                // each peer lives on and can address replies back to it.
                let network = value
                    .get("network")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let payload = value.get("payload").cloned().unwrap_or(Value::Null);
                self.handle_channel(channel, from, network, payload).await;
            }
            "video_inbound" => {
                let from = value.get("from").and_then(|v| v.as_str()).unwrap_or("");
                let Some(data) = value.get("data").and_then(|v| v.as_str()) else {
                    return;
                };
                let key = value.get("key").and_then(|v| v.as_bool()).unwrap_or(false);
                let rtp_timestamp = value
                    .get("rtp_timestamp")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;
                let stream = value.get("stream").and_then(|v| v.as_u64()).unwrap_or(0) as u8;
                self.handle_video_inbound(from, stream, rtp_timestamp, key, data);
            }
            "audio_inbound" => {
                let from = value.get("from").and_then(|v| v.as_str()).unwrap_or("");
                let Some(data) = value.get("data").and_then(|v| v.as_str()) else {
                    return;
                };
                let stream = value.get("stream").and_then(|v| v.as_u64()).unwrap_or(0) as u8;
                self.handle_audio_inbound(from, stream, data);
            }
            "event" => {
                if let Some(event) = value.get("event") {
                    // Connection establishment is a claim-status trigger: a
                    // peer just went live for app traffic ("approved"), so
                    // re-assert presence + fleet roster straight at it —
                    // there is no heartbeat to catch it up later.
                    let approved = event.get("event_kind").and_then(|v| v.as_str()) == Some("peer")
                        && event.get("kind").and_then(|v| v.as_str()) == Some("approved");
                    if approved {
                        if let Some(device) = event.get("device_id").and_then(|v| v.as_str()) {
                            let mesh = self.clone();
                            let device = device.to_string();
                            crate::spawn(async move {
                                mesh.ownership_check(Some(&device)).await;
                            });
                        }
                    }
                    self.sink.emit("allmystuff://event", event.clone());
                }
            }
            _ => {}
        }
    }

    async fn handle_channel(
        self: &Arc<Self>,
        channel: &str,
        from: String,
        network: String,
        payload: Value,
    ) {
        // Remember which network this peer is reachable on, so control/media
        // we send back goes to the right one (a peer may share only one of the
        // several networks we're on).
        if !network.is_empty() && !from.is_empty() {
            self.state
                .lock()
                .peer_networks
                .insert(pubkey_part(&from).to_string(), network);
        }
        match channel {
            CHANNEL_PRESENCE => {
                if let Ok(profile) = serde_json::from_value::<NodeProfile>(payload) {
                    // We answer a peer's presence with our own (+ roster) when
                    // either it's the first we've heard of them this session or
                    // their app just (re)started — so the bootstrap is mutual
                    // even when our earlier advert raced their subscription and
                    // was dropped. This (plus the connection-approved trigger)
                    // is what replaced the periodic re-broadcast; the reply
                    // can't loop because once we each hold the other's presence
                    // neither condition fires again. `boot == 0` is an older
                    // heartbeating peer. Our own echo never replies to itself.
                    let canon = pubkey_part(profile.node.as_str()).to_string();
                    self.state
                        .lock()
                        .peer_features
                        .insert(canon.clone(), profile.features.clone());
                    let is_self = self
                        .local_node_id()
                        .is_some_and(|me| pubkey_part(&me) == canon);
                    let new_boot = profile.boot != 0 && !is_self && {
                        let mut st = self.state.lock();
                        st.peer_boots.insert(canon, profile.boot) != Some(profile.boot)
                    };
                    // Whether this peer's presence was already on file *before*
                    // we fold in this advert. A peer we don't yet know gets an
                    // answer regardless of boot id, so a single dropped first
                    // reply self-heals on their next frame instead of waiting
                    // for a manual network refresh.
                    let node_id = profile.node.clone();
                    // What this device says about its own ownership, captured
                    // before the advert is folded in (moved): used to self-heal
                    // our fleet roster below.
                    let advertised_owner = profile
                        .owner
                        .as_ref()
                        .map(|o| pubkey_part(o.as_str()).to_string());
                    let known = {
                        let st = self.state.lock();
                        st.session
                            .as_ref()
                            .is_some_and(|s| s.peer(&node_id).is_some())
                    };
                    let changed = {
                        let mut st = self.state.lock();
                        st.session
                            .as_mut()
                            .map(|s| s.apply_presence(profile))
                            .unwrap_or(false)
                    };
                    // Self-heal the fleet: if a device we still list as a fleet
                    // member now advertises a *different* owner (or none — it
                    // went unclaimed), it has left us or been re-claimed. Evict
                    // it so the roster reflects reality even when the explicit
                    // leave notification never arrived (it was offline, crashed,
                    // or was claimed straight out from under us).
                    if !is_self && self.ownership.is_fleet_owner() {
                        let me = self.local_node_id().map(|m| pubkey_part(&m).to_string());
                        let peer = pubkey_part(node_id.as_str()).to_string();
                        let in_my_fleet = self
                            .ownership
                            .fleet_member_ids()
                            .iter()
                            .any(|d| pubkey_part(d) == peer)
                            || self.fleet_authorized.lock().contains(&peer);
                        let still_ours = advertised_owner.as_deref() == me.as_deref();
                        if in_my_fleet && !still_ours {
                            tracing::info!(
                                "fleet member {} now answers to a different owner — dropping",
                                short_id(node_id.as_str())
                            );
                            self.fleet_drop_member(node_id.to_string()).await;
                        } else if in_my_fleet && still_ours && (new_boot || !known) {
                            // A member that's still ours just (re)appeared. If the
                            // original fleet-key handoff was lost — we were offline
                            // when it accepted the claim, or the frame dropped — it's
                            // claimed-but-keyless and stuck outside the closed
                            // network. Re-hand the key now; the member's
                            // `adopt_fleet_key` is a no-op when it already holds it,
                            // so this is safe to repeat on every (re)appearance and
                            // self-heals the handoff without a manual nudge. Gated on
                            // the member still being in *our* roster so it never
                            // undoes an eviction (an evicted device we dropped is no
                            // longer `in_my_fleet`, so it isn't re-keyed).
                            tracing::info!(
                                "fleet member {} (re)appeared — re-handing the fleet key in case it was missed",
                                short_id(node_id.as_str())
                            );
                            self.send_fleet_key(node_id.as_str()).await;
                        }
                    }
                    if new_boot || (!is_self && !known) {
                        tracing::info!(
                            "peer {} {} — answering with our presence + roster",
                            short_id(&from),
                            if new_boot {
                                "(re)started"
                            } else {
                                "is new to us"
                            }
                        );
                        self.ownership_check(Some(&from)).await;
                    }
                    if changed {
                        self.emit_snapshot();
                    }
                }
            }
            CHANNEL_CONTROL => {
                if let Ok(msg) = serde_json::from_value::<ControlMessage>(payload) {
                    // Terminal and files offers are screened *before* the
                    // session sees them: the session auto-accepts (Accept +
                    // StartMedia in one step), and a shell — or this disk —
                    // is owner/fleet-only, the same rule as input injection,
                    // enforced before any reply exists.
                    if let ControlMessage::Route(RouteControl::Offer { route, .. }) = &msg {
                        // Log every inbound offer at the point it's received, so
                        // a host's node log shows whether an offer even arrived
                        // (vs. an offerer stuck "awaiting accept" because nothing
                        // here ever processed it). The accept itself is silent
                        // otherwise; a refusal logs the warn below.
                        tracing::info!(
                            route = %route.id,
                            from = %short_id(&from),
                            "route offer received"
                        );
                        let hosts_here = self
                            .local_node_id()
                            .is_some_and(|me| node_of(route.from.as_str()) == me);
                        if let Some(reason) = privileged_offer_refusal(
                            route,
                            hosts_here,
                            self.sender_may_control(&from),
                        ) {
                            tracing::warn!(
                                "privileged offer {} from {} refused: not owner/fleet",
                                route.id,
                                short_id(&from)
                            );
                            let _ = self
                                .send_control(
                                    &from,
                                    &ControlMessage::Route(RouteControl::Reject {
                                        route_id: route.id.clone(),
                                        reason,
                                    }),
                                )
                                .await;
                            return;
                        }
                    }
                    // Site management (list a co-owned machine's sites,
                    // re-expose them) and the terminal-sessions picker plane
                    // (list this host's open shells, the host's answer) ride
                    // this channel but are the backend's to handle, gated
                    // owner/fleet — the session never sees them.
                    match msg {
                        ControlMessage::Site(sc) => {
                            self.handle_site_control(&from, sc).await;
                        }
                        ControlMessage::Route(RouteControl::TerminalSessionsRequest) => {
                            self.handle_terminal_sessions_request(&from).await;
                        }
                        ControlMessage::Route(RouteControl::TerminalSessions { sessions }) => {
                            // A host's answer to *our* picker request — surface
                            // it to the front-end (it picks one to attach to).
                            self.sink.emit(
                                "allmystuff://terminal-sessions",
                                json!({ "from": from, "sessions": sessions }),
                            );
                        }
                        msg => {
                            let effects = {
                                let mut st = self.state.lock();
                                st.session
                                    .as_mut()
                                    .map(|s| s.handle(NodeId::from(from.as_str()), msg))
                                    .unwrap_or_default()
                            };
                            self.process_effects(effects).await;
                            self.emit_snapshot();
                        }
                    }
                }
            }
            CHANNEL_MEDIA => {
                let Some(media) = MediaPayload::decode(payload) else {
                    return;
                };
                match media {
                    MediaPayload::Audio(frame) => self.audio.feed(&frame.route, &frame),
                    MediaPayload::Video(frame) => {
                        // Surface frames only for a route this session knows
                        // is live, sinks here, and belongs to the sender —
                        // the watching window (console stage, room tile)
                        // renders them. Display and camera routes share the
                        // frame shape. Chunked frames reassemble first; the
                        // first complete frame of a stream is logged so
                        // "connected but no pixels" is attributable from
                        // this side too.
                        if !self.inbound_video_ok(&frame.route, &from) {
                            tracing::debug!(
                                "dropped video frame for {} from {} (route not live here)",
                                frame.route,
                                short_id(&from)
                            );
                            return;
                        }
                        let full = { self.video_in.lock().push(frame) };
                        if let Some(full) = full {
                            if full.seq == 0 {
                                tracing::info!(
                                    "first video frame for {} ({}×{})",
                                    full.route,
                                    full.width,
                                    full.height
                                );
                            }
                            self.note_video_in(&full.route, "MJPEG", full.jpeg.len());
                            self.enqueue_for_watcher(&full.route, video_ipc_bytes(&full));
                        }
                    }
                    MediaPayload::VideoStatus(status) => {
                        // The host explaining its capture state ("display
                        // asleep", "camera failed"…). Gated like the frames
                        // it stands in for; the console window shows it on
                        // the stage.
                        if !self.inbound_video_ok(&status.route, &from) {
                            return;
                        }
                        tracing::info!(
                            "capture status for {}: {:?}{}",
                            status.route,
                            status.state,
                            status
                                .detail
                                .as_deref()
                                .map(|d| format!(" ({d})"))
                                .unwrap_or_default()
                        );
                        self.sink.emit(
                            "allmystuff://video-status",
                            serde_json::json!({
                                "route": status.route,
                                "state": status.state,
                                "detail": status.detail,
                            }),
                        );
                    }
                    MediaPayload::Input(ev) => {
                        // Injecting keystrokes is the most privileged thing
                        // on the mesh, so it takes both gates: a live input
                        // route from this exact sender, *and* the sender
                        // being this device's recorded owner or a co-owned
                        // fleet member. (Share-grant-based control rides on
                        // the share enforcement work — not wired yet.)
                        if self.inbound_media_ok(&ev.route, &from, MediaKind::Input)
                            && self.sender_may_control(&from)
                        {
                            self.injector.apply(&ev.route, ev.action);
                        } else {
                            tracing::warn!(
                                "dropped input event from {from}: not an authorized controller"
                            );
                        }
                    }
                    MediaPayload::Terminal(frame) => self.handle_term_frame(&from, frame),
                    MediaPayload::File(frame) => self.handle_file_frame(&from, frame),
                    MediaPayload::Clipboard(frame) => self.handle_clipboard_frame(&from, frame),
                    MediaPayload::Site(frame) => self.handle_site_frame(&from, frame),
                }
            }
            CHANNEL_ROOMS => {
                // The rooms plane is deliberately thin backend-side: rooms
                // live in the GUI (like relationships do), so a decoded
                // message is simply forwarded to every window. Decoding
                // here rather than passing raw JSON keeps the same skew
                // discipline as every other channel — a message this build
                // doesn't understand is dropped, never an error.
                if let Ok(msg) = serde_json::from_value::<RoomMessage>(payload) {
                    self.sink
                        .emit("allmystuff://room", json!({ "from": from, "message": msg }));
                }
            }
            _ => {}
        }
    }

    /// Drop the per-route video state a route that just ended leaves
    /// behind — its receive-side counters, any pending re-key ask, and its
    /// native decoder. (Lane assignment is computed on the fly now, so
    /// there's no per-route lane mapping to free.)
    fn release_video_lanes(&self, route_id: &str) {
        self.video_in_stats.lock().remove(route_id);
        self.refresh_asks.lock().remove(route_id);
        self.video_decode.stop(route_id);
    }

    /// The audio twin of [`Self::release_video_lanes`]: drop the route's
    /// Opus decoder when it ends.
    fn release_audio_lanes(&self, route_id: &str) {
        self.audio_decoders.lock().remove(route_id);
    }

    /// One Opus frame arrived on a peer's audio lane `stream`. It belongs
    /// to whichever of our routes maps to that lane (the lane-th Opus route
    /// from this peer in sorted order — [`Self::audio_route_for_lane`]),
    /// gated exactly like every other media frame (route live, sinks here,
    /// sender is the route's peer) — then decodes straight into the
    /// route's playback ring.
    fn handle_audio_inbound(self: &Arc<Self>, from: &str, stream: u8, data_b64: &str) {
        use base64::Engine as _;
        let Some(route_id) = self.audio_route_for_lane(from, stream) else {
            // The audio twin of the video lane's "no route for it" warn
            // (rate-limited the same way): Opus arriving with nowhere to
            // decode it is the caller-hears-nothing drop, and it used to be
            // a DEBUG whisper while the room sat silent.
            if self.diag_ok(&format!("audio-lane:{}:{stream}", pubkey_part(from))) {
                tracing::warn!(
                    "Opus frames arriving from {} on lane {stream} but no route maps to it — dropped (caller hears nothing)",
                    short_id(from)
                );
            }
            return;
        };
        if !self.inbound_media_ok(&route_id, from, MediaKind::Audio) {
            tracing::debug!("audio frame for {route_id} refused (route not live here)");
            return;
        }
        let Ok(data) = base64::engine::general_purpose::STANDARD.decode(data_b64) else {
            return;
        };
        // Up to 120 ms per packet is legal Opus; ours are 20 ms.
        let mut pcm = vec![0i16; crate::audio::OPUS_FRAME_SAMPLES * 6];
        let decoded = {
            let mut decoders = self.audio_decoders.lock();
            let dec = match decoders.entry(route_id.clone()) {
                std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::hash_map::Entry::Vacant(v) => {
                    match opus::Decoder::new(crate::audio::OPUS_RATE, opus::Channels::Mono) {
                        Ok(d) => v.insert(d),
                        Err(e) => {
                            tracing::warn!("opus decoder for {route_id} failed: {e}");
                            return;
                        }
                    }
                }
            };
            match dec.decode(&data, &mut pcm, false) {
                Ok(n) => n,
                Err(e) => {
                    // One bad frame costs 20 ms; the next stands alone.
                    tracing::debug!("opus decode for {route_id} failed: {e}");
                    return;
                }
            }
        };
        pcm.truncate(decoded);
        let frame = AudioFrame::new(route_id.clone(), 0, crate::audio::OPUS_RATE, 1, pcm);
        self.audio.feed(&route_id, &frame);
    }

    /// Count one inbound video payload for `route_id` and emit the
    /// receive-side dial-in line every few seconds:
    /// `video in <route>: 28.4 fps · 4.1 Mbps · H.264`.
    fn note_video_in(&self, route_id: &str, label: &'static str, bytes: usize) {
        const EVERY: std::time::Duration = std::time::Duration::from_secs(5);
        let mut map = self.video_in_stats.lock();
        let st = map
            .entry(route_id.to_string())
            .or_insert_with(|| VideoInStats::new(label));
        st.label = label;
        st.frames += 1;
        st.bytes += bytes as u64;
        let elapsed = st.since.elapsed();
        if elapsed >= EVERY {
            let secs = elapsed.as_secs_f64();
            let line = format!(
                "video in {route_id}: {:.1} fps · {:.1} Mbps · {}",
                st.frames as f64 / secs,
                (st.bytes as f64 * 8.0) / secs / 1_000_000.0,
                st.label,
            );
            if crate::video::stats_to_info() {
                tracing::info!("{line}");
            } else {
                tracing::debug!("{line}");
            }
            st.since = std::time::Instant::now();
            st.frames = 0;
            st.bytes = 0;
        }
    }

    /// One assembled H.264 access unit arrived on a peer's track lane
    /// `stream`. It belongs to whichever of our routes maps to that lane
    /// (the lane-th H.264 route from this peer in sorted order —
    /// [`Self::video_route_for_lane`]), gated exactly like MJPEG frames
    /// (route live, sinks here, sender is the route's peer) before it
    /// reaches a console window. Where it goes next is the watcher's
    /// choice: access units straight through (the webview decodes —
    /// WebCodecs), or through the native decoder, which hands the window
    /// ready-to-paint RGBA frames.
    fn handle_video_inbound(
        self: &Arc<Self>,
        from: &str,
        stream: u8,
        rtp_timestamp: u32,
        key: bool,
        data_b64: &str,
    ) {
        use base64::Engine as _;
        let canon = pubkey_part(from).to_string();
        let Some(route_id) = self.video_route_for_lane(from, stream) else {
            // The sender is streaming the track lane at us but no route
            // here maps to it — the one-sided stream the viewer reads as
            // "connecting forever". Loud (rate-limited): this exact drop
            // was a debug whisper while the stage sat black.
            if self.diag_ok(&format!("lane:{canon}:{stream}")) {
                tracing::warn!(
                    "H.264 samples arriving from {} on lane {stream} but no route maps to it — dropped (viewer shows nothing)",
                    short_id(from)
                );
            }
            return;
        };
        if !self.inbound_video_ok(&route_id, from) {
            if self.diag_ok(&format!("gate:{route_id}")) {
                tracing::warn!(
                    "H.264 samples for {route_id} refused — {}",
                    self.route_diag(&route_id, from)
                );
            }
            return;
        }
        let Ok(data) = base64::engine::general_purpose::STANDARD.decode(data_b64) else {
            return;
        };
        // The arrival side of the sender's "route active — streaming"
        // line: one INFO per stream, so a healthy hop is attributable
        // from this end too (the MJPEG path has logged its first frame
        // this way all along).
        let first = !self.video_in_stats.lock().contains_key(&route_id);
        self.note_video_in(&route_id, "H.264", data.len());
        let wants_decode = self
            .video_watchers
            .lock()
            .get(&route_id)
            .is_some_and(|w| w.decode);
        if first {
            tracing::info!(
                "first H.264 sample for {route_id} from {} ({} bytes, key={key}, native decode={wants_decode})",
                short_id(from),
                data.len(),
            );
        }
        // 90 kHz RTP clock → µs for the decoder's timestamps.
        let ts_us = rtp_timestamp as u64 * 1000 / 90;
        if wants_decode {
            let mesh = Arc::downgrade(self);
            let rid = route_id.clone();
            let glitch_mesh = Arc::downgrade(self);
            let glitch_rid = route_id.clone();
            self.video_decode.feed(
                &route_id,
                Au { ts_us, key, data },
                move |packet| {
                    if let Some(mesh) = mesh.upgrade() {
                        mesh.enqueue_decoded(&rid, packet);
                    }
                },
                move || {
                    // The native decoder hit a corrupt unit or dumped its
                    // queue: ask the sender to re-key rather than waiting
                    // out the periodic IDR (rate-limited inside).
                    if let Some(mesh) = glitch_mesh.upgrade() {
                        let rid = glitch_rid.clone();
                        crate::spawn(async move {
                            let _ = mesh.request_refresh(rid).await;
                        });
                    }
                },
            );
        } else {
            self.enqueue_for_watcher(&route_id, h264_ipc_bytes(ts_us, key, &data));
        }
    }

    /// Queue one packet for a watching console window; drop the packet
    /// (with a debug note) when no window watches the route. A queue
    /// nobody drains (webview wedged or closing) caps at a few seconds
    /// of stream and is then cleared wholesale — the decoder re-keys on
    /// the sender's next IDR, and `video_unwatch`/route teardown remove
    /// the entry entirely.
    fn enqueue_for_watcher(&self, route_id: &str, packet: Vec<u8>) {
        const MAX_QUEUED: usize = 120;
        let mut map = self.video_watchers.lock();
        let Some(w) = map.get_mut(route_id) else {
            drop(map);
            // Routine for a beat while a window boots; a *persistent* run
            // of these is a stream with nowhere to land — say so at a
            // visible level (rate-limited) instead of the debug whisper
            // that read as a silent black stage.
            if self.diag_ok(&format!("watchless:{route_id}")) {
                tracing::info!(
                    "frames flowing for {route_id} but no window is watching it — dropping until one does"
                );
            }
            return;
        };
        if w.queue.len() >= MAX_QUEUED {
            tracing::debug!("video queue for {route_id} unread for seconds — cleared");
            w.queue.clear();
        }
        w.queue.push_back(packet);
        // Poke the watcher when the queue goes non-empty: the console
        // pulls on a timer, but Chromium throttles timers in occluded
        // windows (a non-maximized console behind the main window paints
        // ~1 fps) — the event rides eval, which isn't throttled, and it
        // also shaves the poll interval off delivery latency. Coalesced
        // by construction: no further pokes until the queue drains.
        if w.queue.len() == 1 {
            self.sink.emit("allmystuff://video-ready", json!(route_id));
        }
    }

    /// Queue one natively decoded frame, freshest-wins: a decoded picture
    /// supersedes anything the window hasn't pulled yet (each is a complete
    /// screen — painting two per tick buys nothing but latency). Encoded
    /// packets append instead, because H.264 deltas must all reach their
    /// decoder; that distinction is the whole reason for two enqueues.
    fn enqueue_decoded(&self, route_id: &str, packet: Vec<u8>) {
        let mut map = self.video_watchers.lock();
        let Some(w) = map.get_mut(route_id) else {
            tracing::debug!("no console window watching {route_id} — decoded frame dropped");
            return;
        };
        w.queue.clear();
        w.queue.push_back(packet);
        self.sink.emit("allmystuff://video-ready", json!(route_id));
    }

    /// Front-end command: offer a route from `from` to `to`.
    pub async fn connect(
        self: &Arc<Self>,
        from: String,
        to: String,
        media: String,
        video: Vec<String>,
    ) -> Result<String, String> {
        self.connect_term(from, to, media, video, None).await
    }

    /// [`connect`](Self::connect) with an optional terminal **session** to
    /// attach to (the multi-attach entry point): `Some(id)` makes the
    /// terminal Offer name that already-running host shell to join, `None`
    /// (and every non-terminal route) mints a fresh session as before.
    pub async fn connect_term(
        self: &Arc<Self>,
        from: String,
        to: String,
        media: String,
        video: Vec<String>,
        session: Option<String>,
    ) -> Result<String, String> {
        // Only advertise transports the *whole* local stack can consume.
        // H.264 decode is always covered (WebCodecs where the webview has
        // it, the native decoder where it doesn't) — but inbound samples
        // arrive via the daemon, and an old one would negotiate a stream
        // it can't deliver.
        let video = if self.daemon_video.load(Ordering::SeqCst) {
            video
        } else {
            Vec::new()
        };
        let me = self.local_node_id().ok_or("mesh not ready")?;
        let media = parse_media(&media);
        let route = Route {
            id: format!("route:{from}→{to}"),
            from: from.clone().into(),
            to: to.clone().into(),
            media,
        };
        let from_node = node_of(&from);
        let to_node = node_of(&to);
        // Audio accepts mirror video's: when we're the *sink* of an audio
        // route and our daemon speaks the audio lane, ask for Opus — the
        // source side picks the lane when its own stack can carry it,
        // and PCM frames over the media channel stay the floor.
        let audio = if media == MediaKind::Audio
            && to_node == me
            && self.daemon_audio.load(Ordering::SeqCst)
        {
            vec!["opus".to_string()]
        } else {
            Vec::new()
        };
        let peer = if from_node == me { to_node } else { from_node };

        if peer == me {
            // Local loopback (e.g. this machine's mic to its own speakers):
            // no peer to negotiate with — record it active and stream now.
            // Offer-then-Accept drives the session to Active and yields the
            // StartMedia effect we process below.
            let effects = {
                let mut st = self.state.lock();
                let s = st.session.as_mut().ok_or("mesh not ready")?;
                // Loopback terminals carry the attach session too, so two
                // local windows can share one local shell (multi-attach to
                // yourself); harmless `None` on every other loopback route.
                let _ = s.offer_terminal(
                    route.clone(),
                    me.as_str(),
                    Vec::new(),
                    Vec::new(),
                    session.clone(),
                );
                s.handle(
                    NodeId::from(me.as_str()),
                    ControlMessage::Route(RouteControl::Accept {
                        route_id: route.id.clone(),
                        session: None,
                    }),
                )
            };
            self.process_effects(effects).await;
            self.emit_snapshot();
            return Ok(route.id);
        }

        let msg = {
            let mut st = self.state.lock();
            let s = st.session.as_mut().ok_or("mesh not ready")?;
            s.offer_terminal(route.clone(), peer.as_str(), video, audio, session)
        };
        if let Err(e) = self.send_control(&peer, &msg).await {
            // The peer never saw the offer — drop it rather than leave a
            // phantom half-open route in the session.
            tracing::warn!(
                "route {} offer to {} undeliverable: {e}",
                route.id,
                short_id(&peer)
            );
            let mut st = self.state.lock();
            if let Some(s) = st.session.as_mut() {
                let _ = s.teardown(&route.id);
            }
            return Err(e);
        }
        // The accept lands moments later as the route's "active" line; an
        // offer that goes nowhere has its own warns above and below. At
        // INFO (not DEBUG) so a default-level capture shows the whole
        // offer → accept → active arc — the silence after this line is the
        // tell when a route is offered but the peer never accepts.
        tracing::info!(
            "route {} offered to {} — awaiting accept",
            route.id,
            short_id(&peer)
        );
        self.emit_snapshot();
        Ok(route.id)
    }

    /// Register interest in one route's inbound frames (replacing any
    /// previous watcher — the route shows in one window). Packets queue
    /// from this moment; the window drains them with [`Self::video_poll`].
    /// `decode` asks the backend to run inbound H.264 through the native
    /// decoder and queue ready-to-paint RGBA frames instead of access
    /// units — for webviews without WebCodecs, and the last rung of the
    /// console's decode ladder. Returns the claim token to pass back to
    /// [`Self::video_unwatch`].
    pub fn video_watch(&self, route_id: String, decode: bool) -> u64 {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let token = NEXT.fetch_add(1, Ordering::Relaxed);
        if !decode {
            // A pass-through watcher replacing a decoding one (input
            // switch, ladder reset) leaves no orphan decoder behind.
            self.video_decode.stop(&route_id);
        }
        // One line per watch claim, so a viewer-side log shows which
        // window holds each stream and on which decode path — the missing
        // half of "frames flowing but no window watching".
        tracing::info!("window watching {route_id} (native decode: {decode})");
        self.video_watchers.lock().insert(
            route_id,
            VideoWatcher {
                token,
                decode,
                queue: std::collections::VecDeque::new(),
            },
        );
        token
    }

    /// Release a watch claim — only if `token` still owns the route. A
    /// late unwatch from a replaced watcher is a no-op instead of
    /// deleting its successor's queue.
    pub fn video_unwatch(&self, route_id: &str, token: u64) {
        let mut map = self.video_watchers.lock();
        if map.get(route_id).is_some_and(|w| w.token == token) {
            map.remove(route_id);
            drop(map);
            self.video_decode.stop(route_id);
        }
    }

    /// Drain everything queued for `route_id` into one length-prefixed
    /// batch: `[u32 len][packet]…` — empty (and cheap) when nothing
    /// arrived since the last poll.
    pub fn video_poll(&self, route_id: &str) -> Vec<u8> {
        let mut map = self.video_watchers.lock();
        let Some(w) = map.get_mut(route_id) else {
            return Vec::new();
        };
        let total: usize = w.queue.iter().map(|p| 4 + p.len()).sum();
        let mut out = Vec::with_capacity(total);
        for packet in w.queue.drain(..) {
            out.extend_from_slice(&(packet.len() as u32).to_le_bytes());
            out.extend_from_slice(&packet);
        }
        out
    }

    pub async fn disconnect(self: &Arc<Self>, route_id: String) -> Result<(), String> {
        let msg = {
            let mut st = self.state.lock();
            st.session.as_mut().and_then(|s| s.teardown(&route_id))
        };
        self.audio.stop(&route_id);
        self.video.stop(&route_id);
        self.video_watchers.lock().remove(&route_id);
        self.release_video_lanes(&route_id);
        self.release_audio_lanes(&route_id);
        self.terminal.stop(&route_id);
        self.files.stop(&route_id);
        // The unmapping (client) side gets no local StopMedia effect — only
        // the wire Teardown goes out — so close the listener + connections
        // here, or they'd leak (the port stays bound, the accept loop runs).
        self.sites.stop_route(&route_id);
        self.drop_downloads(&route_id);
        if let (Some(msg), Some(peer)) = (&msg, self.route_peer(&route_id)) {
            // Best-effort: the route is gone locally either way.
            let _ = self.send_control(&peer, msg).await;
        }
        self.emit_snapshot();
        Ok(())
    }

    pub fn snapshot(&self) -> Value {
        let st = self.state.lock();
        let Some(session) = st.session.as_ref() else {
            return json!({ "ready": false });
        };
        let me = session.me().to_string();
        let network = st.network.clone();
        let peers: Vec<_> = session.peers().collect();
        let routes: Vec<_> = session.routes().collect();
        // Durable shares (person + unioned grants) so the GUI reclassifies a
        // peer as *shared* with its grants across a restart, rather than
        // forgetting them and defaulting to unclaimed.
        let shares = self.shares.shares();
        json!({
            "ready": true,
            "me": me,
            "network": network,
            "peers": peers,
            "routes": routes,
            "shares": shares,
        })
    }

    fn route_peer(&self, route_id: &str) -> Option<String> {
        self.state
            .lock()
            .session
            .as_ref()
            .and_then(|s| s.route(route_id).map(|r| r.peer.to_string()))
    }

    // ---- shares (durable, person-scoped grants) -----------------------
    //
    // The GUI resolves the person + node and hands them down; the node is the
    // source of truth (enforcement lives here) and the next [`Mesh::snapshot`]
    // reflects the change. These persist *my* policy so it survives a restart
    // **and** tell the peer over the control channel, so a share is a mutual
    // fact rather than one-sided local policy — what made it no better than a
    // room. The send is best-effort: the durable record is written first, so a
    // peer that's offline now just isn't notified yet (the local policy still
    // holds, and a later phase re-asserts on reconnect).

    /// Record an **outbound** grant — what this person may do with my stuff —
    /// persist it, and offer my full current grant set to their device.
    pub async fn share_grant(
        &self,
        person: Person,
        node: NodeId,
        grant: Grant,
    ) -> Result<(), String> {
        self.shares.grant(&person, &node, grant);
        self.emit_snapshot();
        self.send_share_invite(&person, &node).await
    }

    /// Tell `node` the full set of grants this person currently holds from me.
    /// Sent whole because the peer records inbound by **replacement**, so the
    /// complete set is the authoritative "here's everything you may do".
    async fn send_share_invite(&self, person: &Person, node: &NodeId) -> Result<(), String> {
        let grants = self.shares.out_grants_for(&person.id);
        let msg = ControlMessage::Share(ShareControl::Invite {
            from: self.local_person(),
            grants,
        });
        self.send_control(node.as_str(), &msg).await
    }

    /// Revoke a grant by its (content-derived) id from a person's share, and
    /// tell every device they bring to drop it too (revocation is unilateral —
    /// the content-derived id names the same grant on both ends).
    pub async fn share_revoke(&self, person: PersonId, grant_id: String) -> Result<(), String> {
        self.shares.revoke(&person, &grant_id);
        self.emit_snapshot();
        let mut last_err = None;
        for node in self.shares.nodes_for(&person) {
            let msg = ControlMessage::Share(ShareControl::Revoke {
                grant_id: grant_id.clone(),
            });
            if let Err(e) = self.send_control(node.as_str(), &msg).await {
                last_err = Some(e);
            }
        }
        last_err.map_or(Ok(()), Err)
    }

    /// Stop sharing with a person entirely — drop the whole durable record and
    /// revoke each outbound grant on their devices (captured before the drop).
    pub async fn share_stop(&self, person: PersonId) -> Result<(), String> {
        let nodes = self.shares.nodes_for(&person);
        let grant_ids: Vec<String> = self
            .shares
            .out_grants_for(&person)
            .into_iter()
            .map(|g| g.id)
            .collect();
        self.shares.stop_sharing(&person);
        self.emit_snapshot();
        for node in &nodes {
            for grant_id in &grant_ids {
                let msg = ControlMessage::Share(ShareControl::Revoke {
                    grant_id: grant_id.clone(),
                });
                let _ = self.send_control(node.as_str(), &msg).await;
            }
        }
        Ok(())
    }

    /// This machine's owner-or-self as a graph [`Person`] — the identity an
    /// outbound [`ShareControl::Invite`] carries. Keyed `person:<pubkey>` to
    /// mirror the GUI's `person:<owner ?? self>`, so both ends agree on "me".
    fn local_person(&self) -> Person {
        let me = self.local_node_id().unwrap_or_default();
        let owner = self.ownership.owner().unwrap_or_else(|| me.clone());
        Person {
            id: format!("person:{}", pubkey_part(&owner)).into(),
            name: self
                .profile_label()
                .unwrap_or_else(|| me.chars().take(10).collect()),
        }
    }

    /// The [`Person`] we attribute an inbound share to — keyed by the
    /// **authenticated** sender's pubkey, *never* the self-asserted body id.
    /// This is the load-bearing trust rule: an inbound offer can only ever bind
    /// the sender's own node into the sender's own share, so a peer can't slip
    /// its node into someone else's person (which later outbound enforcement
    /// would otherwise trust). The body supplies only a display name.
    fn peer_person(&self, from: &NodeId, name: Option<&str>) -> Person {
        let display = name
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| self.peer_label(from));
        Person {
            id: format!("person:{}", pubkey_part(from.as_str())).into(),
            name: display,
        }
    }

    /// Apply an inbound share-control message. Unlike app-control or a
    /// privileged offer, this is **not** gated on `sender_may_control`: a share
    /// is person-to-person, so the sharer is never the recipient's owner/fleet.
    /// The mesh's ed25519 handshake already authenticates `from`; recording
    /// what they offer is safe because an *inbound* grant only ever widens what
    /// *I* may pull from *them*, never what they may do to me (that direction is
    /// my own outbound grant, minted only by my explicit action).
    async fn handle_share(&self, from: NodeId, message: ShareControl) {
        match message {
            ShareControl::Invite { from: body, grants } => {
                let person = self.peer_person(&from, Some(&body.name));
                self.shares.record_inbound(&person, &from, grants);
                self.emit_snapshot();
                self.sink.emit(
                    "allmystuff://share",
                    json!({ "from": from.to_string(), "kind": "invite", "person": person.name }),
                );
                // Acknowledge, carrying any grants I already extend back — so
                // sharing can be mutual in one round trip (empty if I've granted
                // them nothing; the ack never *mints* an outbound grant).
                let back = self.shares.out_grants_for(&person.id);
                let reply = ControlMessage::Share(ShareControl::Accept { grants: back });
                if let Err(e) = self.send_control(from.as_str(), &reply).await {
                    tracing::warn!("couldn't ack share from {}: {e}", short_id(from.as_str()));
                }
            }
            ShareControl::Accept { grants } => {
                let person = self.peer_person(&from, None);
                self.shares.record_inbound(&person, &from, grants);
                self.emit_snapshot();
                self.sink.emit(
                    "allmystuff://share",
                    json!({ "from": from.to_string(), "kind": "accept", "person": person.name }),
                );
            }
            ShareControl::Decline => {
                tracing::info!("share declined by {}", short_id(from.as_str()));
                self.sink.emit(
                    "allmystuff://share",
                    json!({ "from": from.to_string(), "kind": "decline" }),
                );
            }
            ShareControl::Revoke { grant_id } => {
                let person = self.peer_person(&from, None);
                self.shares.revoke(&person.id, &grant_id);
                self.emit_snapshot();
                self.sink.emit(
                    "allmystuff://share",
                    json!({ "from": from.to_string(), "kind": "revoke" }),
                );
            }
            // A share-control kind a newer build introduced — nothing to do.
            ShareControl::Unknown => {}
        }
    }

    async fn process_effects(self: &Arc<Self>, effects: Vec<Effect>) {
        for e in effects {
            match e {
                Effect::Send { peer, message } => {
                    // Replies ride best-effort; the failure is already logged.
                    let _ = self.send_control(&peer.to_string(), &message).await;
                }
                Effect::StartMedia(route) => self.start_media(&route),
                Effect::RefreshMedia(id) => self.video.force_idr(&id),
                Effect::TuneMedia {
                    route_id,
                    max_edge,
                    bitrate,
                    fps,
                } => self.video.retune(
                    &route_id,
                    crate::video::Tune {
                        max_edge,
                        bitrate,
                        fps,
                    },
                ),
                Effect::VideoFeedback {
                    route_id,
                    recv_fps,
                    decode_fails,
                    queue_depth,
                } => self
                    .video
                    .note_feedback(&route_id, recv_fps, decode_fails, queue_depth),
                Effect::StopMedia(id) => {
                    self.audio.stop(&id);
                    self.video.stop(&id);
                    self.video_watchers.lock().remove(&id);
                    self.release_video_lanes(&id);
                    self.release_audio_lanes(&id);
                    // A control route ending mid-chord must not leave this
                    // machine holding the keys it injected.
                    self.injector.release_route(&id);
                    // A terminal route ending is one *viewer* leaving, not the
                    // shell dying: detach (keep the shared shell alive for the
                    // other attachers, host or remote; the last one leaving
                    // arms the idle reaper), never kill. Closing a tab on one
                    // machine must not end a session another still has open.
                    self.terminal.detach(&id);
                    // Drop this route's terminal pump/dedup bookkeeping so a
                    // later route reusing the id starts clean (and the maps
                    // never grow unbounded over a long session).
                    self.term_pumps.lock().remove(&id);
                    self.term_rx_seq.lock().remove(&id);
                    self.term_in_seq.lock().remove(&id);
                    self.files.stop(&id);
                    // A site route ending closes its local listener (client
                    // side) and every tunneled connection it carried.
                    self.sites.stop_route(&id);
                    self.drop_downloads(&id);
                }
                Effect::Share { from, message } => self.handle_share(from, message).await,
                Effect::Ownership { from, message } => self.handle_ownership(from, message).await,
                Effect::App { from, message } => self.handle_app_control(from, message).await,
            }
        }
    }

    /// Apply an inbound app-control command. These are fleet-only — a machine
    /// only acts on the say-so of its owner or a fleet co-member (the same
    /// rule a terminal/remote-control offer is screened by), so a command
    /// from anyone else is logged and dropped.
    async fn handle_app_control(self: &Arc<Self>, from: NodeId, message: AppControl) {
        if !self.sender_may_control(from.as_str()) {
            tracing::warn!(
                "app-control {:?} from {} ignored: not owner/fleet",
                message,
                short_id(from.as_str())
            );
            return;
        }
        match message {
            AppControl::Upgrade => {
                tracing::info!(
                    "upgrade requested by {} — running self-update",
                    short_id(from.as_str())
                );
                // Download + apply off the inbound-frame task (it does network
                // I/O), then restart onto the new build. The peer gets no
                // reply: our next presence advert (the new version) is the
                // confirmation, and the button it pressed disappears when the
                // upgrade lands — exactly how a claim confirms by re-advert.
                let sink = self.sink.clone();
                crate::spawn(async move {
                    match allmystuff_updater::update_now().await {
                        Ok(allmystuff_updater::UpdateNowOutcome::Updated { to, components }) => {
                            tracing::info!(
                                "self-update applied {to} ({}) — restarting",
                                components.join("+")
                            );
                            sink.restart();
                        }
                        Ok(other) => {
                            tracing::info!("upgrade request: nothing to do ({other:?})")
                        }
                        Err(e) => tracing::warn!("upgrade request failed: {e}"),
                    }
                });
            }
            // An app command a newer build introduced that this one doesn't
            // implement (decoded as `Unknown` rather than failing the
            // control message) — nothing to act on.
            AppControl::Unknown => {}
        }
    }

    /// Apply an inbound ownership message. A [`OwnershipControl::Claim`] is
    /// the load-bearing one: this device only lets the claim take if it's
    /// actually claimable (in claim mode and unowned) — that's the rule that
    /// stops a peer flat-out taking a box. The other variants are feedback
    /// the claimer's UI surfaces.
    async fn handle_ownership(self: &Arc<Self>, from: NodeId, message: OwnershipControl) {
        match message {
            OwnershipControl::Claim { owner } => {
                // The owner of record is the *authenticated sender* the mesh
                // delivered (`from`), never an arbitrary value in the body —
                // otherwise a peer could claim a box "for" someone else. The
                // claimer asserts its display id while the daemon delivers the
                // bare pubkey, so compare by pubkey (self-asserted) and record
                // the authenticated `from`.
                let reply = if pubkey_part(owner.as_str()) != pubkey_part(from.as_str()) {
                    OwnershipControl::Declined {
                        reason: "a claim must be self-asserted".into(),
                    }
                } else if self.ownership.try_accept_claim(from.as_str()) {
                    // The claim took — a claim change runs the full status
                    // check: re-advertise with the new owner so the claimer
                    // (and everyone) sees it's now spoken for. Any stale
                    // fleet state was reset by the accept; the owner's
                    // roster lands next on the owned channel.
                    tracing::info!(
                        "claim accepted: {} now owns this device",
                        short_id(from.as_str())
                    );
                    self.ownership_check(None).await;
                    // Push our own owned roster now so this device's GUI knows
                    // it's claimed (in a fleet) immediately — before the owner's
                    // `FleetKey` handoff lands. Without this, an owned-but-keyless
                    // window would read as "not in a fleet" while we'd already
                    // refuse to be made claimable, the very contradiction the
                    // roster's `claimed` flag exists to resolve.
                    self.emit_owned().await;
                    OwnershipControl::Claimed { owner }
                } else {
                    tracing::info!(
                        "claim from {} declined: not in claim mode",
                        short_id(from.as_str())
                    );
                    OwnershipControl::Declined {
                        reason: "this device isn't in claim mode".into(),
                    }
                };
                if let Err(e) = self
                    .send_control(&from.to_string(), &ControlMessage::Ownership(reply))
                    .await
                {
                    tracing::warn!(
                        "couldn't send the claim reply to {}: {e}",
                        short_id(from.as_str())
                    );
                }
            }
            OwnershipControl::Release => {
                // The recorded owner is letting this device go (compare by
                // pubkey — same display-vs-bare id reconciliation as Claim).
                // This also covers a kick: the owner sends Release alongside
                // the closed-network Evict so the device ejects itself even if
                // it missed (or won't honour) the signed removal.
                let owner = self.ownership.owner();
                if owner.as_deref().map(pubkey_part) == Some(pubkey_part(from.as_str())) {
                    tracing::info!("released by {} — unowned again", short_id(from.as_str()));
                    // Tear out of the fleet's closed network before clearing the
                    // credential (set_owner(None) drops the key it derives from).
                    let fleet_net = self.ownership.fleet_network_id();
                    self.ownership.set_owner(None);
                    if let Some(network) = fleet_net {
                        let _ = self
                            .client
                            .request(&Request::NetworkRemove { network })
                            .await;
                    }
                    self.refresh_fleet_authorization().await;
                    self.ownership_check(None).await;
                }
            }
            OwnershipControl::Claimed { owner } => {
                // The device we claimed (`from`) accepted us as its owner.
                // Make the claim *do* something durable: mint our fleet key on
                // the first adoption, record ourselves and the new device in
                // the owner's re-admit list, found the fleet's closed network
                // (electing us Owner) and admit the new device into its signed
                // roster, then hand the fleet key down to it so it derives and
                // joins the same network. The signed roster — not gossip — is
                // now the authority for membership and control.
                let key = self.ownership.ensure_fleet_key();
                if let Some(me) = self.local_node_id() {
                    let my_label = self.profile_label().unwrap_or_else(|| me.clone());
                    self.ownership.upsert_member(&me, &my_label);
                }
                let label = self.peer_label(&from);
                self.ownership.upsert_member(from.as_str(), &label);
                tracing::info!(
                    "claim confirmed by {}; fleet key …{} now {} member(s)",
                    short_id(from.as_str()),
                    key_tail(&key),
                    self.ownership.fleet_member_ids().len(),
                );
                // Found the closed network (if new) and admit every member —
                // including the one just claimed — into its signed roster.
                self.ensure_fleet_network().await;
                self.refresh_fleet_authorization().await;
                // Hand the new device its fleet credential point-to-point so it
                // joins the same closed network and converges its roster.
                self.send_fleet_key(from.as_str()).await;
                self.emit_owned().await;
                // Surface the claim feedback for the claimer's toast, too.
                self.sink.emit(
                    "allmystuff://ownership",
                    json!({
                        "from": from.to_string(),
                        "message": OwnershipControl::Claimed { owner },
                    }),
                );
            }
            OwnershipControl::FleetKey { key, name, venue } => {
                // Our owner handed us the fleet credential. Adopt the key (so we
                // derive the same closed network), join it, and converge our
                // signed roster from the owner's governance. Only honoured from
                // our recorded owner — a stray key from anyone else is ignored.
                let from_is_owner = self.ownership.owner().as_deref().map(pubkey_part)
                    == Some(pubkey_part(from.as_str()));
                if !from_is_owner {
                    tracing::warn!(
                        "ignoring fleet key from {} — not our owner",
                        short_id(from.as_str())
                    );
                    return;
                }
                if self.ownership.adopt_fleet_key(&key, &name) {
                    tracing::info!(
                        "adopted fleet key …{} from {} — joining its closed network",
                        key_tail(&key),
                        short_id(from.as_str())
                    );
                    self.ensure_fleet_network().await;
                    self.refresh_fleet_authorization().await;
                    self.emit_owned().await;
                }
                // Apply the owner's venue regardless of whether the key changed —
                // the owner may have re-handed it *only* to update the venue (a
                // venue change re-broadcasts with the same key+name).
                if let Some(venue) = venue {
                    self.apply_fleet_venue(&venue).await;
                }
            }
            OwnershipControl::FleetDeparted => {
                // A member is telling us it left the fleet. Evict it from the
                // signed roster so our view (and every other member's) reflects
                // reality. Only the fleet owner acts on this.
                if self.ownership.is_fleet_owner() {
                    tracing::info!(
                        "{} left the fleet — dropping from the roster",
                        short_id(from.as_str())
                    );
                    self.fleet_drop_member(from.to_string()).await;
                }
            }
            other => {
                // Declined — feedback for the claimer's UI.
                tracing::info!(
                    "ownership reply from {}: {:?}",
                    short_id(from.as_str()),
                    other
                );
                self.sink.emit(
                    "allmystuff://ownership",
                    json!({ "from": from.to_string(), "message": other }),
                );
            }
        }
    }

    /// Re-stamp the live presence profile's owner/claimable from the store
    /// and broadcast, so an ownership change propagates immediately.
    async fn refresh_profile_ownership(self: &Arc<Self>) {
        {
            let mut st = self.state.lock();
            if let Some(p) = st.profile.as_mut() {
                p.owner = self.ownership.owner().map(NodeId::from);
                p.claimable = self.ownership.claimable();
            }
        }
        self.broadcast_presence().await;
        self.emit_snapshot();
    }

    // ---- owned fleet gossip ------------------------------------------

    /// This node's current display label from the live presence profile.
    fn profile_label(&self) -> Option<String> {
        self.state.lock().profile.as_ref().map(|p| p.label.clone())
    }

    /// Best-known display label for a peer (matched by canonical pubkey, since
    /// the daemon delivers a bare pubkey while presence is keyed by display
    /// id), else a short id. Gives fleet members a friendly name.
    fn peer_label(&self, peer: &NodeId) -> String {
        let canon = pubkey_part(peer.as_str());
        {
            let st = self.state.lock();
            if let Some(session) = st.session.as_ref() {
                for p in session.peers() {
                    if pubkey_part(p.node.as_str()) == canon && !p.label.trim().is_empty() {
                        return p.label.clone();
                    }
                }
            }
        }
        let s = peer.as_str();
        if s.len() > 12 {
            format!("{}…", &s[..10])
        } else {
            s.to_string()
        }
    }

    /// Hand a freshly-claimed device its fleet credential point-to-point: the
    /// shared key (so it derives the same closed-network id and joins it) and
    /// the fleet name. This replaces the old gossiped `OwnedRoster` — the
    /// device's signed-roster membership converges from the owner's governance
    /// once it's in the network.
    async fn send_fleet_key(&self, peer: &str) {
        let Some(key) = self.ownership.fleet_key() else {
            return;
        };
        let name = self.ownership.fleet_name();
        // Hand the fleet's venue (transport servers) down with the key, so the
        // member rides the same calling-out point as the rest of the fleet.
        let venue = self.fleet_venue_json().await;
        let msg = ControlMessage::Ownership(OwnershipControl::FleetKey { key, name, venue });
        match self.send_control(peer, &msg).await {
            Ok(()) => tracing::info!("handed the fleet key to {}", short_id(peer)),
            Err(e) => tracing::warn!("couldn't hand the fleet key to {}: {e}", short_id(peer)),
        }
    }

    /// The owner's fleet-network **venue** — its transport servers (signaling /
    /// STUN / TURN) — as a JSON object string, read from the live daemon config,
    /// to hand a member so it calls out where the fleet does. Just the transport
    /// fields; the member owns its own id/label/kind. `None` when the fleet
    /// network isn't configured yet or carries no servers (defaults are fine).
    async fn fleet_venue_json(&self) -> Option<String> {
        let network = self.ownership.fleet_network_id()?;
        let resp = self.client.request(&Request::ConfigShow).await.ok()?;
        if !resp.ok {
            return None;
        }
        let data = resp.data?;
        let nets = data.pointer("/config/networks")?.as_array()?;
        let cfg = nets.iter().find(|n| {
            let id = n.get("id").and_then(|v| v.as_str()).unwrap_or_default();
            let nid = n
                .get("network_id")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            id == network || nid == network
        })?;
        let mut venue = serde_json::Map::new();
        for k in ["signaling", "stun_servers", "turn_servers"] {
            if let Some(v) = cfg.get(k) {
                venue.insert(k.to_string(), v.clone());
            }
        }
        if venue.is_empty() {
            return None;
        }
        serde_json::to_string(&Value::Object(venue)).ok()
    }

    /// Apply the owner's handed-down fleet **venue** to this device's fleet
    /// network, so it calls out where the rest of the fleet does. Members mirror
    /// the owner's venue; they don't define it. A best-effort `NetworkUpdate`
    /// over just the transport fields, keyed to our own fleet network id.
    async fn apply_fleet_venue(self: &Arc<Self>, venue_json: &str) {
        let Some(network) = self.ownership.fleet_network_id() else {
            return;
        };
        let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(venue_json) else {
            return;
        };
        let mut config = serde_json::Map::new();
        config.insert("id".into(), Value::String(network.clone()));
        config.insert("network_id".into(), Value::String(network.clone()));
        for (k, v) in obj {
            config.insert(k, v);
        }
        let _ = self
            .client
            .request(&Request::NetworkUpdate {
                config: Value::Object(config),
            })
            .await;
        self.sync_networks().await;
    }

    /// Whether `network` is this device's fleet mesh.
    pub fn is_fleet_network(&self, network: &str) -> bool {
        self.ownership.fleet_network_id().as_deref() == Some(network)
    }

    /// Owner-only: re-hand the fleet key — which now carries the fleet-network
    /// venue — to every member, so a venue the owner just changed propagates to
    /// the whole fleet. A no-op for a non-owner: members don't define the venue,
    /// only the owner broadcasts it (managers manage members, not core settings).
    pub async fn fleet_broadcast_config(self: &Arc<Self>) {
        if !self.ownership.is_fleet_owner() {
            return;
        }
        let me = self.local_node_id().map(|m| pubkey_part(&m).to_string());
        for member in self.ownership.fleet_member_ids() {
            if Some(pubkey_part(&member).to_string()) == me {
                continue;
            }
            self.send_fleet_key(&member).await;
        }
    }

    /// Push the current fleet roster to the front-end. Sourced from the
    /// closed network's **signed roster**, so the GUI shows authenticated
    /// membership, not a gossiped guess.
    async fn emit_owned(&self) {
        let value = self.fleet_roster_value().await;
        self.sink.emit("allmystuff://owned", value);
    }

    /// The current fleet roster as JSON — for the `owned_roster` command and
    /// the `allmystuff://owned` event, in the `OwnedRoster` shape the GUI
    /// expects: the shared key + name from local state, members from the
    /// fleet's closed-network **signed roster** (`RosterList`). An empty
    /// key/members when there's no fleet yet, so the front-end always gets a
    /// well-formed shape.
    pub async fn fleet_roster_value(&self) -> Value {
        // The single membership truth the whole GUI reads: `in_fleet`. A device
        // is in a fleet the moment it's claimed — it belongs to its owner's
        // fleet even before the owner's `FleetKey` handoff lands (which can lag
        // or fail if the owner is briefly offline) — or whenever it holds a key.
        // The GUI never sees the *local* node's own `owner`, so it leans on this
        // one flag; every place that asks "am I in a fleet" (the drawer, the
        // settings pane, the leave button) reads it, so they can't disagree.
        let in_fleet = self.ownership.in_fleet();
        // Not in a fleet at all → the empty, well-formed shape. Everything below
        // assumes membership, and the GUI keys solely on `in_fleet`.
        if !in_fleet {
            let mut v = empty_owned();
            if let Some(o) = v.as_object_mut() {
                o.insert("in_fleet".into(), Value::Bool(false));
            }
            return v;
        }
        // In a fleet. The key/network may be absent — an owned-but-keyless
        // member that's been claimed but hasn't received its owner's key
        // handoff is in a fleet with no closed network of its own yet. In that
        // case there's no signed roster to read; the membership the user sees is
        // still real (self, plus the owner's local list when we're the owner).
        let key = self.ownership.fleet_key().unwrap_or_default();
        let network = self.ownership.fleet_network_id();
        let mut members: Vec<OwnedMember> = Vec::new();
        let mut member_roles: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        if let Some(network) = network.as_deref() {
            if let Ok(r) = self
                .client
                .request(&Request::RosterList {
                    network: network.to_string(),
                })
                .await
            {
                if r.ok {
                    if let Some(arr) = r
                        .data
                        .as_ref()
                        .and_then(|d| d.get("roster"))
                        .and_then(|v| v.as_array())
                    {
                        for e in arr {
                            if let Some(id) = e.get("device_id").and_then(|v| v.as_str()) {
                                let label = e
                                    .get("label")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                // The governance role projection ("member" /
                                // "controller" / "owner"), so the GUI can label
                                // the grant/withdraw controls per member.
                                let role = e
                                    .get("role")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("member")
                                    .to_string();
                                member_roles.insert(pubkey_part(id).to_string(), role);
                                members.push(OwnedMember {
                                    device: NodeId::from(pubkey_part(id)),
                                    label,
                                });
                            }
                        }
                    }
                }
            }
        }
        // Fold in the owner's durable local member list so its devices show as
        // members immediately — before the closed network's signed roster
        // re-converges on startup, and through a transient roster-read failure —
        // and so the roster the GUI sees matches the owner's actual membership
        // rather than diverging from it. A left or evicted device is dropped
        // from this list too (the removal paths clear both), so the merge never
        // resurrects one; a non-owner member's list is empty, a no-op there.
        for m in self.ownership.fleet_members() {
            let canon = pubkey_part(m.device.as_str()).to_string();
            if !members
                .iter()
                .any(|x| pubkey_part(x.device.as_str()) == canon)
            {
                members.push(OwnedMember {
                    device: NodeId::from(canon.as_str()),
                    label: m.label.clone(),
                });
            }
            member_roles
                .entry(canon)
                .or_insert_with(|| "member".to_string());
        }
        // The signed roster a node holds never lists *itself* — each device is
        // locally authoritative and isn't re-added from a peer's roster gossip
        // (MyOwnMesh `on_roster_entries` skips the self entry). But the fleet
        // the user sees includes this device: it holds the key. Add self so the
        // GUI's "am I in my fleet" check (and the relationship reconcile that
        // depends on it) is true for members, not just the owner.
        // This device's own role (it's never in the signed roster it holds).
        // Read from the governance state so a promoted manager shows as such,
        // not as a plain member, then mapped back to MyOwnMesh's wire term
        // ("manager" → "controller") for the roster the GUI reads.
        let self_role_wire = match self.self_fleet_role().await.as_deref() {
            Some("owner") => "owner",
            Some("manager") => "controller",
            _ if self.ownership.is_fleet_owner() => "owner",
            _ => "member",
        };
        if let Some(me) = self.local_node_id() {
            let canon = pubkey_part(&me).to_string();
            if !members
                .iter()
                .any(|m| pubkey_part(m.device.as_str()) == canon)
            {
                let label = self.profile_label().unwrap_or_else(|| me.clone());
                members.push(OwnedMember {
                    device: NodeId::from(canon.as_str()),
                    label,
                });
            }
            member_roles
                .entry(canon)
                .or_insert_with(|| self_role_wire.to_string());
        }
        let roster = OwnedRoster {
            key,
            name: self.ownership.fleet_name(),
            version: self.ownership.fleet_version(),
            members,
        };
        let mut value = serde_json::to_value(roster).unwrap_or_else(|_| empty_owned());
        if let Some(obj) = value.as_object_mut() {
            // Tag whether *this* device is the fleet owner (the founder /
            // key-holder), so the GUI can gate owner-only actions.
            obj.insert(
                "is_owner".into(),
                Value::Bool(self.ownership.is_fleet_owner()),
            );
            // The fleet's closed-network id, so the GUI can spot which mesh in
            // the list is the fleet mesh and lock it (you leave it by leaving
            // the fleet, not by removing the mesh). Empty for a keyless member
            // that hasn't joined a closed network yet.
            obj.insert(
                "network_id".into(),
                Value::String(network.unwrap_or_default()),
            );
            // The single membership flag — always true here (we returned early
            // when not in a fleet), so the GUI's "am I in a fleet" check is the
            // same regardless of whether we hold a key yet.
            obj.insert("in_fleet".into(), Value::Bool(in_fleet));
            // Stamp each member with its governance role for the drawer's
            // grant/withdraw controls.
            if let Some(arr) = obj.get_mut("members").and_then(|v| v.as_array_mut()) {
                for m in arr {
                    let canon = m
                        .get("device")
                        .and_then(|v| v.as_str())
                        .map(|d| pubkey_part(d).to_string())
                        .unwrap_or_default();
                    let role = member_roles
                        .get(&canon)
                        .cloned()
                        .unwrap_or_else(|| "member".to_string());
                    if let Some(mo) = m.as_object_mut() {
                        mo.insert("role".into(), Value::String(role));
                    }
                }
            }
        }
        value
    }

    /// Front-end command: claim `node` as owned by this device. Only the
    /// target deciding it's claimable makes it stick; we just send intent —
    /// but a send the daemon couldn't deliver (device dropped offline, no
    /// shared network) is surfaced so the UI can say so rather than leaving
    /// "asking…" hanging forever.
    pub async fn claim(self: &Arc<Self>, node: String) -> Result<(), String> {
        let me = self.local_node_id().ok_or("mesh not ready")?;
        tracing::info!("claiming {} (sending ownership claim)", short_id(&node));
        let msg = ControlMessage::Ownership(OwnershipControl::Claim { owner: me.into() });
        self.send_control(&node, &msg).await
    }

    /// Front-end command: ask a fleet machine to update itself to the
    /// channel's latest release and restart. The far side enforces owner/fleet
    /// before acting (and decides there's nothing to do if it's already
    /// current); its next presence advert — carrying the new version — is the
    /// confirmation. A send the daemon couldn't deliver is surfaced so the UI
    /// can say so rather than leaving the ask hanging.
    pub async fn request_upgrade(self: &Arc<Self>, node: String) -> Result<(), String> {
        tracing::info!("asking {} to upgrade + restart", short_id(&node));
        let msg = ControlMessage::App(AppControl::Upgrade);
        self.send_control(&node, &msg).await
    }

    /// Front-end command: put *this* device into (or out of) claim mode, so
    /// another of your machines can adopt it. Re-advertises immediately.
    pub async fn set_claimable(self: &Arc<Self>, on: bool) -> Result<bool, String> {
        self.ownership.set_claim_mode(on);
        self.refresh_profile_ownership().await;
        Ok(self.ownership.claimable())
    }

    /// The closed network backing this device's fleet (derived from the fleet
    /// key). The GUI targets the fleet's custody-MFA enroll/status at this id.
    pub fn fleet_network_id(&self) -> Option<String> {
        self.ownership.fleet_network_id()
    }

    /// The claim-status check — "is what we believe about ownership still
    /// true, and does everyone else know it?" Re-stamps the live profile from
    /// the ownership store, then re-asserts presence. Runs **targeted** at one
    /// peer right after its connection establishes or its app (re)starts — so
    /// the two sides converge on the event itself; there is no heartbeat — and
    /// **broadcast** on the local triggers: session start, a claim/release,
    /// and fleet membership changes.
    pub async fn ownership_check(self: &Arc<Self>, peer: Option<&str>) {
        if self.local_node_id().is_none() {
            return;
        }
        {
            let mut st = self.state.lock();
            if let Some(p) = st.profile.as_mut() {
                p.owner = self.ownership.owner().map(NodeId::from);
                p.claimable = self.ownership.claimable();
            }
        }
        match peer {
            Some(peer) => {
                tracing::debug!("ownership check → {}", short_id(peer));
                self.send_presence_to(peer).await;
            }
            None => {
                self.broadcast_presence().await;
            }
        }
        self.emit_owned().await;
        self.emit_snapshot();

        // Keep the closed-network fleet and its signed-roster cache in step
        // with this ownership change. Founding (owner-side `NetworkAdd` +
        // founder self-election + member admits) runs on the
        // broadcast/startup/claim path only; the authorised-controller cache
        // refresh runs on every check.
        if peer.is_none() {
            self.ensure_fleet_network().await;
        }
        self.refresh_fleet_authorization().await;
    }

    /// Make sure the fleet's closed network exists, is genuinely closed, and
    /// its signed roster reflects the fleet.
    ///
    /// Both sides `NetworkAdd` the network as **open** first — seeding it
    /// closed would block the founder self-election, which is only valid
    /// `open → closed`. The **owner** then proposes the `KindChange → closed`
    /// (a single-signer founder self-election that auto-ratifies, electing it
    /// Owner and making governance genuinely closed — without which the roles
    /// map stays empty and fleet-MFA guards nothing), and admits every member
    /// into the signed roster. A **member** just joins open and converges to
    /// closed from the owner's broadcast governance. All steps are idempotent;
    /// best-effort, with failures logged by the daemon.
    async fn ensure_fleet_network(self: &Arc<Self>) {
        let Some(network) = self.ownership.fleet_network_id() else {
            return;
        };
        let config = json!({
            "id": network.as_str(),
            "network_id": network.as_str(),
            "label": fleet_label(&self.ownership.fleet_name()),
            "kind": "open",
        });
        // A duplicate `NetworkAdd` (already joined) returns an error we ignore.
        let _ = self.client.request(&Request::NetworkAdd { config }).await;

        // Keep the fleet-mesh **label** converged. `NetworkAdd` is a no-op once
        // joined, so it never refreshes the label — but a rename handed down to a
        // member arrives as a fresh key+name and re-runs this. Without an explicit
        // update the member's fleet-mesh pill (and anywhere the mesh label titles
        // things) would keep the old name even though its graph fleet-name pill,
        // fed by the roster, already updated. NetworkUpdate makes the owner's
        // rename actually spread to every member's mesh label too.
        let label = fleet_label(&self.ownership.fleet_name());
        let _ = self
            .client
            .request(&Request::NetworkUpdate {
                config: json!({
                    "id": network.as_str(),
                    "network_id": network.as_str(),
                    "label": label,
                }),
            })
            .await;

        // The set of joined networks just changed — pick the fleet network up
        // everywhere: refresh `st.networks`, (re)subscribe its channels, and
        // re-advertise the `allmystuff` capability + presence on it. Without
        // this the joiner is on the fleet mesh but never advertises the app tag
        // there, so peers (e.g. the owner whose graph centres on this network)
        // see it connected-but-mesh-only — "online, not on AllMyStuff" — until
        // some unrelated network change happens to trigger a sync.
        self.sync_networks().await;

        if !self.ownership.is_fleet_owner() {
            // A member pre-rosters its **owner**. Fleet membership is mutual
            // trust established by the claim, but MyOwnMesh only auto-approves a
            // connection from a peer that's already in your roster — so without
            // this the member would be prompted to "let in" its own owner (and
            // approving it would admit the owner via the handshake). The owner
            // already pre-rosters the member at claim time; this is the
            // symmetric half. We trust our owner inherently (it owns us), so
            // there's no authority gap — and this never propagates (a member
            // isn't a roster-grant authority, so peers refuse its roster gossip).
            if let Some(owner) = self.ownership.owner() {
                let _ = self
                    .client
                    .request(&Request::RosterApprove {
                        network: network.clone(),
                        device_id: pubkey_part(&owner).to_string(),
                        label: None,
                    })
                    .await;
            }
            return;
        }

        // Found the closed governance if it isn't already ours. Only propose
        // when the network is still open — a redundant propose on an
        // already-closed network would sit in pending forever (propose doesn't
        // pre-validate). `is_fleet_founded` reads the signed state.
        if !self.is_fleet_founded(&network).await {
            match self
                .client
                .request(&Request::GovernanceProposeKindChange {
                    network: network.clone(),
                    to: "closed".into(),
                    mfa_code: None,
                })
                .await
            {
                Ok(r) if r.ok => {
                    tracing::info!("founded fleet closed network {network} (self-elected owner)")
                }
                Ok(r) => tracing::warn!(
                    "founding fleet network {network} refused: {}",
                    r.error.unwrap_or_else(|| "(no error)".into())
                ),
                Err(e) => tracing::warn!("founding fleet network {network} failed: {e}"),
            }
        }

        // Admit every fleet member into the signed roster — the authenticated
        // membership `sender_may_control` trusts. Idempotent re-approve keeps
        // the roster (and its gossip to members) converged across restarts.
        for member in self.ownership.fleet_member_ids() {
            let _ = self
                .client
                .request(&Request::RosterApprove {
                    network: network.clone(),
                    device_id: pubkey_part(&member).to_string(),
                    label: None,
                })
                .await;
        }
    }

    /// Whether this device already holds the founder-Owner role on the fleet's
    /// closed network — i.e. the `KindChange → closed` self-election has
    /// ratified. Reads the signed governance state; on any error assumes
    /// not-yet-founded (a redundant propose is cheaper to avoid than a missed
    /// one is to recover). `me` is matched in bare-pubkey form, as the roles
    /// map keys it.
    async fn is_fleet_founded(self: &Arc<Self>, network: &str) -> bool {
        let Some(me) = self.local_node_id() else {
            return false;
        };
        let me = pubkey_part(&me).to_string();
        let data = match self
            .client
            .request(&Request::GovernanceState {
                network: network.to_string(),
            })
            .await
        {
            Ok(r) if r.ok => r.data.unwrap_or(Value::Null),
            _ => return false,
        };
        let state = data.get("state").unwrap_or(&Value::Null);
        let closed = state.get("kind").and_then(|v| v.as_str()) == Some("closed");
        let i_am_owner = state
            .get("roles")
            .and_then(|v| v.as_object())
            .and_then(|roles| {
                roles
                    .iter()
                    .find(|(k, _)| pubkey_part(k) == me)
                    .map(|(_, v)| v.as_str() == Some("owner"))
            })
            .unwrap_or(false);
        closed && i_am_owner
    }

    /// This device's own governance role on its fleet's closed network, in the
    /// AllMyStuff UI terms — "owner" | "manager" | "member". A node's own entry
    /// is never in the signed roster it holds, so the role is read from the
    /// signed governance state's `roles` map (MyOwnMesh's "controller" surfaces
    /// as the friendlier "manager"). The founder always reads as owner, even
    /// before the kind-change self-election ratifies. `None` when not in a fleet
    /// (or no closed network yet — a claimed-but-keyless member), where there's
    /// nothing to grant anyway. The daemon is the security boundary; this is the
    /// local view that gates the friendlier, narrower checks on top of it.
    async fn self_fleet_role(&self) -> Option<String> {
        let network = self.ownership.fleet_network_id()?;
        if self.ownership.is_fleet_owner() {
            return Some("owner".to_string());
        }
        let me = self.local_node_id()?;
        let me = pubkey_part(&me).to_string();
        let data = match self
            .client
            .request(&Request::GovernanceState { network })
            .await
        {
            Ok(r) if r.ok => r.data.unwrap_or(Value::Null),
            // Daemon unreachable / no state yet: assume the lowest authority
            // rather than guess upward — a member can't grant, which fails safe.
            _ => return Some("member".to_string()),
        };
        let role = data
            .get("state")
            .and_then(|s| s.get("roles"))
            .and_then(|v| v.as_object())
            .and_then(|roles| {
                roles
                    .iter()
                    .find(|(k, _)| pubkey_part(k) == me)
                    .and_then(|(_, v)| v.as_str())
                    .map(|r| match r {
                        "owner" => "owner",
                        "controller" => "manager",
                        _ => "member",
                    })
            })
            .unwrap_or("member");
        Some(role.to_string())
    }

    /// Refresh the authorised-controller cache ([`Mesh::fleet_authorized`])
    /// from the fleet's closed-network **signed roster** (`RosterList`). No
    /// fleet → clear it (only the owner, via the direct check in
    /// `sender_may_control`, may control). Daemon unreachable → keep the prior
    /// cache rather than briefly denying a legitimate controller.
    async fn refresh_fleet_authorization(self: &Arc<Self>) {
        let Some(network) = self.ownership.fleet_network_id() else {
            self.fleet_authorized.lock().clear();
            return;
        };
        let data = match self.client.request(&Request::RosterList { network }).await {
            Ok(r) if r.ok => r.data.unwrap_or(Value::Null),
            _ => return,
        };
        let mut set = std::collections::HashSet::new();
        if let Some(arr) = data.get("roster").and_then(|v| v.as_array()) {
            for e in arr {
                if let Some(id) = e.get("device_id").and_then(|v| v.as_str()) {
                    set.insert(pubkey_part(id).to_string());
                }
            }
        }
        *self.fleet_authorized.lock() = set;
    }

    /// Send this node's presence profile straight to one peer — the
    /// targeted half of `broadcast_presence`, for a peer that just
    /// connected or restarted and so has never heard us.
    async fn send_presence_to(&self, peer: &str) {
        let profile = { self.state.lock().profile.clone() };
        let Some(profile) = profile else { return };
        let Some(network) = self.network_for_peer(peer) else {
            return;
        };
        if let Ok(payload) = serde_json::to_value(&profile) {
            let _ = self
                .client
                .request(&Request::ChannelSendTo {
                    network,
                    channel: CHANNEL_PRESENCE.to_string(),
                    peer: pubkey_part(peer).to_string(),
                    payload,
                })
                .await;
        }
    }

    /// Front-end command: leave the fleet this device belongs to. Tell the
    /// owner first (so it evicts us from the signed roster instead of believing
    /// we're still a member — the leave-side mirror of the owner's kick), then
    /// drop the local credential, tear out of the fleet's closed network, and —
    /// since membership follows ownership — let any recorded owner go and
    /// re-advertise unowned.
    pub async fn fleet_leave(self: &Arc<Self>) -> Result<(), String> {
        // Notify *before* we leave the network, while we can still route a
        // control frame on the fleet mesh.
        if self.ownership.is_fleet_owner() {
            // We're the owner dissolving our own fleet — there's no owner to
            // tell. Tell every member to release instead, so they stop deriving
            // the (now-defunct) closed network and showing each other as fleet.
            // Best-effort per member (mirrors fleet_kick's direct Release); an
            // offline member just keeps a dead key until it next reconciles.
            for member in self.ownership.fleet_member_ids() {
                let _ = self
                    .send_control(
                        &member,
                        &ControlMessage::Ownership(OwnershipControl::Release),
                    )
                    .await;
            }
        } else if let Some(owner) = self.ownership.owner() {
            // We're a member: tell the owner so it evicts us from the signed
            // roster. Best-effort — surface the failure (don't swallow it) so
            // it's diagnosable; our re-advertised "unowned" presence below is
            // the backstop (the owner drops a member that answers to a
            // different owner / none).
            if let Err(e) = self
                .send_control(
                    &owner,
                    &ControlMessage::Ownership(OwnershipControl::FleetDeparted),
                )
                .await
            {
                tracing::warn!(
                    "couldn't tell the fleet owner we left ({e}); relying on our unowned re-advert to clear us from its roster"
                );
            }
        }
        // Leaving clears all local fleet/ownership state atomically (owner
        // included). It returns the closed network to tear out of, or `None`
        // when there was no key to derive one (an owned-but-keyless member that
        // never joined a network — it has still left); `Err` only when there
        // was genuinely nothing to leave.
        let network = self.ownership.leave_fleet()?;
        if let Some(network) = network {
            tracing::info!("leaving the fleet — forgetting closed network {network}");
            let _ = self
                .client
                .request(&Request::NetworkRemove { network })
                .await;
        } else {
            tracing::info!(
                "left the fleet (was claimed but keyless — no closed network to forget)"
            );
        }
        self.refresh_fleet_authorization().await;
        self.refresh_profile_ownership().await;
        self.emit_owned().await;
        Ok(())
    }

    /// Front-end command: kick `device` out of the fleet. Only the fleet
    /// **owner** can — eviction is an owner-authority governance act on the
    /// closed network. The signed `Evict` propagates the removal to every
    /// member (so the device loses control authorisation everywhere, even if
    /// it's lost/stolen), and a best-effort `Release` tells a cooperative
    /// device to eject itself immediately. `code` is the owner's custody
    /// second factor when fleet MFA is enrolled (the GUI prompts for it);
    /// otherwise it's `None`.
    pub async fn fleet_kick(
        self: &Arc<Self>,
        device: String,
        code: Option<String>,
    ) -> Result<(), String> {
        if !self.ownership.is_fleet_owner() {
            return Err("only the fleet owner can remove a device".into());
        }
        let network = self.ownership.kick_member(&device)?;
        let target = pubkey_part(&device).to_string();
        tracing::info!(
            "evicting {} from fleet network {network}",
            short_id(&device)
        );
        let resp = self
            .client
            .request(&Request::GovernanceProposeEvict {
                network,
                target,
                mfa_code: code,
            })
            .await;
        match resp {
            Ok(r) if r.ok => {}
            Ok(r) => {
                return Err(r
                    .error
                    .unwrap_or_else(|| "couldn't evict the device".into()))
            }
            Err(e) => return Err(e.to_string()),
        }
        // Tell the device directly too, so a cooperative one ejects at once.
        let _ = self
            .send_control(
                &device,
                &ControlMessage::Ownership(OwnershipControl::Release),
            )
            .await;
        self.refresh_fleet_authorization().await;
        self.emit_owned().await;
        Ok(())
    }

    /// Internal: drop `device` from the fleet *locally* — a plain roster
    /// remove, not the propagating governance `Evict`. Used for automatic
    /// roster cleanup (a member told us it left, or a device reappeared under
    /// a new owner) where there's no user to supply an MFA code and the device
    /// is already gone anyway, so a local removal that keeps the owner's view
    /// honest is the right, friction-free tool. Best-effort.
    async fn fleet_drop_member(self: &Arc<Self>, device: String) {
        let Ok(network) = self.ownership.kick_member(&device) else {
            return;
        };
        let target = pubkey_part(&device).to_string();
        tracing::info!(
            "dropping {} from the fleet roster (local)",
            short_id(&device)
        );
        let _ = self
            .client
            .request(&Request::RosterRemove {
                network,
                device_id: target,
            })
            .await;
        self.refresh_fleet_authorization().await;
        self.emit_owned().await;
    }

    /// Front-end command: name (or rename) the fleet. Owner-authoritative:
    /// the name is set locally, pushed onto the closed network's label, and —
    /// since the owner is the source of truth for the fleet name — re-handed
    /// to every member so it propagates instead of having to be set on each
    /// device. (Members got the name with their fleet key at claim time; a
    /// rename re-sends it.) The UI refreshes from `allmystuff://owned`.
    pub async fn fleet_set_name(self: &Arc<Self>, name: String) -> Result<(), String> {
        self.ownership.set_fleet_name(&name)?;
        tracing::info!("fleet named {:?}", self.ownership.fleet_name());
        if let Some(network) = self.ownership.fleet_network_id() {
            let config = json!({
                "id": network.as_str(),
                "network_id": network.as_str(),
                "label": fleet_label(&self.ownership.fleet_name()),
            });
            let _ = self
                .client
                .request(&Request::NetworkUpdate { config })
                .await;
        }
        // Re-hand the (now-renamed) fleet key to every member so the name
        // converges across the fleet. Owner-only — a member has no members to
        // notify, and the name is the owner's to set.
        if self.ownership.is_fleet_owner() {
            let me = self.local_node_id().map(|m| pubkey_part(&m).to_string());
            for member in self.ownership.fleet_member_ids() {
                if Some(pubkey_part(&member).to_string()) == me {
                    continue;
                }
                self.send_fleet_key(&member).await;
            }
        }
        self.emit_owned().await;
        Ok(())
    }

    /// Front-end command: grant `device` a fleet role. `role` is the AllMyStuff
    /// UI term — "member" (a node that can be controlled), "manager" (a
    /// MyOwnMesh "controller": can control, and can admit members) or "owner"
    /// (controls, and sets managers and owners). AllMyStuff's policy is
    /// deliberately narrower than the MyOwnMesh substrate (see
    /// [`ams_may_grant`]): a manager may grant the **member** role only —
    /// managers and owners are the owner's to set, and an owner grant also needs
    /// a human at an owner device (the custody second factor). We enforce the
    /// product rule here first, for a clear refusal rather than a silent quorum
    /// drop, then float the proposal; the daemon re-enforces the quorum on the
    /// wire as the real security boundary. The roster's role projection updates
    /// once it ratifies, and the GUI refreshes from `allmystuff://owned`.
    pub async fn fleet_grant_role(
        self: &Arc<Self>,
        device: String,
        role: String,
        code: Option<String>,
    ) -> Result<(), String> {
        let network = self
            .ownership
            .fleet_network_id()
            .ok_or("this device isn't in a fleet")?;
        // Normalise the requested UI term ("controller" is accepted as an alias
        // for "manager" so older callers keep working).
        let requested = match role.as_str() {
            "member" => "member",
            "manager" | "controller" => "manager",
            "owner" => "owner",
            other => return Err(format!("unknown fleet role: {other}")),
        };
        // AllMyStuff's narrower-than-MyOwnMesh policy: a manager may grant the
        // member role only. Refuse the rest here, before the daemon even sees
        // the proposal, so the user gets a plain reason.
        let self_role = self
            .self_fleet_role()
            .await
            .unwrap_or_else(|| "member".to_string());
        if !ams_may_grant(&self_role, requested) {
            return Err(match requested {
                "owner" => "only the fleet owner can set owners".to_string(),
                "manager" => "only the fleet owner can set managers".to_string(),
                _ => "you don't have authority to grant that role".to_string(),
            });
        }
        // Map the UI term onto MyOwnMesh's role names ("manager" → "controller").
        let role = match requested {
            "member" => "member",
            "manager" => "controller",
            "owner" => "owner",
            _ => unreachable!("requested normalised above"),
        };
        let target = pubkey_part(&device).to_string();
        tracing::info!("granting {role} to {} on {network}", short_id(&device));
        let resp = self
            .client
            .request(&Request::GovernanceProposeRoleGrant {
                network,
                target,
                role: role.to_string(),
                mfa_code: code,
            })
            .await;
        match resp {
            Ok(r) if r.ok => {}
            Ok(r) => return Err(r.error.unwrap_or_else(|| "couldn't grant the role".into())),
            Err(e) => return Err(e.to_string()),
        }
        self.refresh_fleet_authorization().await;
        self.emit_owned().await;
        Ok(())
    }

    /// Front-end command: withdraw `device`'s fleet role — revoke it back to a
    /// plain member. Used for "withdraw as manager / owner". Like a grant, the
    /// daemon enforces who may revoke (authority over the target's current
    /// role); we float the proposal and surface any refusal.
    pub async fn fleet_revoke_role(
        self: &Arc<Self>,
        device: String,
        code: Option<String>,
    ) -> Result<(), String> {
        let network = self
            .ownership
            .fleet_network_id()
            .ok_or("this device isn't in a fleet")?;
        let target = pubkey_part(&device).to_string();
        tracing::info!("revoking role from {} on {network}", short_id(&device));
        let resp = self
            .client
            .request(&Request::GovernanceProposeRoleRevoke {
                network,
                target,
                mfa_code: code,
            })
            .await;
        match resp {
            Ok(r) if r.ok => {}
            Ok(r) => {
                return Err(r
                    .error
                    .unwrap_or_else(|| "couldn't withdraw the role".into()))
            }
            Err(e) => return Err(e.to_string()),
        }
        self.refresh_fleet_authorization().await;
        self.emit_owned().await;
        Ok(())
    }

    /// Re-read the joined networks, (re)subscribe every channel on each, then
    /// re-advertise. Called after the set of networks changes (create / join /
    /// leave) or a network's transport is restarted by a signaling/STUN/TURN
    /// edit — so the session follows the user across *every* network they're
    /// on, not just the ones present at launch. Re-subscribing an existing
    /// channel is idempotent on the daemon.
    pub async fn sync_networks(self: &Arc<Self>) {
        let client_id = { self.state.lock().client_id };
        let Some(client_id) = client_id else { return };
        let networks = self.fetch_networks().await;
        let primary = networks.first().cloned();
        {
            let mut st = self.state.lock();
            st.networks = networks.clone();
            st.network = primary.clone();
        }
        self.subscribe_channels(client_id, &networks).await;
        self.advertise_capabilities().await;
        self.broadcast_presence().await;
        self.emit_snapshot();
    }

    /// Subscribe presence, control, media, and rooms on each given network.
    /// All of them ride every network: presence broadcasts so peers are found
    /// wherever they are, and point-to-point (control/media/rooms) so a frame
    /// addressed to whichever network the *sender* last saw us on always has a
    /// subscriber here. (The fleet's `OwnedRoster` gossip channel is gone —
    /// membership is the closed network's signed roster now.)
    async fn subscribe_channels(&self, client_id: ClientId, networks: &[String]) {
        let channels = [
            CHANNEL_PRESENCE,
            CHANNEL_CONTROL,
            CHANNEL_MEDIA,
            CHANNEL_ROOMS,
        ];
        for network in networks {
            for channel in channels {
                let _ = self
                    .client
                    .request(&Request::ChannelSubscribe {
                        client_id,
                        network: network.clone(),
                        channel: channel.to_string(),
                    })
                    .await;
            }
            // The video track lane's inbound side: assembled H.264
            // access units arrive as `video_inbound` events. The verdict
            // doubles as the capability probe: a daemon that predates the
            // lane refuses the op, and we pin `daemon_video` accordingly
            // so every transport choice (ours and what we ask peers for)
            // degrades to MJPEG instead of a stream nobody can carry.
            match self
                .client
                .request(&Request::VideoSubscribe {
                    client_id,
                    network: network.clone(),
                })
                .await
            {
                Ok(resp) if resp.ok => {
                    self.daemon_video.store(true, Ordering::SeqCst);
                    // Learn the daemon's media-lane pool size, so we know how many
                    // simultaneous streams to one peer can ride separate lanes.
                    if let Some(n) = self
                        .client
                        .request(&Request::Status)
                        .await
                        .ok()
                        .and_then(|r| r.data)
                        .and_then(|d| d.get("media_lanes").and_then(|v| v.as_u64()))
                    {
                        self.daemon_lanes
                            .store(n.clamp(1, 255) as u8, Ordering::SeqCst);
                    }
                }
                _ => {
                    if !self.daemon_video.load(Ordering::SeqCst) {
                        let version = self
                            .client
                            .request(&Request::Status)
                            .await
                            .ok()
                            .and_then(|r| r.data)
                            .and_then(|d| {
                                d.get("version").and_then(|v| v.as_str()).map(String::from)
                            })
                            .unwrap_or_else(|| "unknown".into());
                        tracing::warn!(
                            "daemon v{version} doesn't speak the video track lane (needs myownmesh ≥ 0.2.1) — screen shares fall back to MJPEG"
                        );
                    }
                }
            }
            // The audio lane's inbound side + capability probe, exactly
            // like video's: a daemon that predates the lane refuses the
            // op, and audio rides PCM frames over the media channel.
            match self
                .client
                .request(&Request::AudioSubscribe {
                    client_id,
                    network: network.clone(),
                })
                .await
            {
                Ok(resp) if resp.ok => {
                    self.daemon_audio.store(true, Ordering::SeqCst);
                }
                _ => {
                    if !self.daemon_audio.load(Ordering::SeqCst) {
                        tracing::info!(
                            "daemon doesn't speak the audio track lane (needs myownmesh ≥ 0.2.4) — audio rides the data channel"
                        );
                    }
                }
            }
        }
    }

    /// Begin carrying media for a now-active route. Audio, display (screen
    /// streaming), video (camera streaming), and input (remote control)
    /// are wired; storage still shows active without a transport, and the
    /// log says so.
    fn start_media(self: &Arc<Self>, route: &Route) {
        let Some(me) = self.local_node_id() else {
            return;
        };
        let from_node = node_of(route.from.as_str());
        let to_node = node_of(route.to.as_str());

        match route.media {
            MediaKind::Audio => {
                // We source: capture what the routed capability names — the
                // machine's own playback for the synthetic `system-audio`,
                // the default mic for a scanned input device — and stream
                // it to the sink. Transport: the offer said what the sink
                // can consume — Opus on the daemon's audio track lane when
                // both stacks carry it and this peer's lane is free, PCM
                // frames over the media channel otherwise (the floor).
                if from_node == me {
                    let source = audio_capture_source(route);
                    let accepts_opus = self
                        .state
                        .lock()
                        .session
                        .as_ref()
                        .and_then(|s| s.route(&route.id))
                        .map(|r| r.audio.iter().any(|a| a == "opus"))
                        .unwrap_or(false);
                    let lane = accepts_opus && self.audio_lane(&route.id, &to_node, true).is_some();
                    tracing::info!(
                        "route {} active — streaming {} to {} ({})",
                        route.id,
                        match source {
                            CaptureSource::System => "system audio",
                            CaptureSource::Mic => "mic audio",
                        },
                        short_id(&to_node),
                        if lane { "Opus lane" } else { "PCM channel" }
                    );
                    let peer = to_node.clone();
                    let tx = self.audio_out.clone();
                    let encoder = if lane {
                        match crate::audio::OpusStream::new() {
                            Ok(enc) => Some(parking_lot::Mutex::new(enc)),
                            Err(e) => {
                                tracing::warn!(
                                    "opus encoder for {} failed ({e}) — falling back to PCM frames",
                                    route.id
                                );
                                self.release_audio_lanes(&route.id);
                                None
                            }
                        }
                    } else {
                        None
                    };
                    let rid = route.id.clone();
                    let seq = Arc::new(AtomicU64::new(0));
                    self.audio
                        .start_capture(route.id.clone(), source, move |pcm, rate| {
                            // try_send everywhere: a full queue drops this
                            // buffer; the next one carries fresher sound.
                            if let Some(enc) = &encoder {
                                enc.lock().push(&pcm, rate, |data| {
                                    let _ = tx.try_send(AudioOut::Lane {
                                        peer: peer.clone(),
                                        route: rid.clone(),
                                        data,
                                    });
                                });
                            } else {
                                let s = seq.fetch_add(1, Ordering::Relaxed);
                                let frame = AudioFrame::new(rid.clone(), s, rate, 1, pcm);
                                let _ = tx.try_send(AudioOut::Channel(peer.clone(), frame));
                            }
                        });
                }
                // We sink: play inbound frames for this route. Inbound Opus
                // lane samples find their route on demand
                // ([`Self::audio_route_for_lane`]) — the peer maps each
                // active-codec route to a lane by sorted position the same
                // way we do, so no claim is recorded here (the sender may
                // still pick PCM, in which case the lane simply never sees a
                // frame).
                if to_node == me {
                    tracing::info!(
                        "route {} active — playing audio from {}",
                        route.id,
                        short_id(&from_node)
                    );
                    self.audio.start_playback(route.id.clone());
                }
            }
            MediaKind::Display => {
                // We're the screen being looked at: capture and stream to
                // the viewer. The transport comes from the offer: when the
                // viewer can decode H.264 and this peer's track lane is
                // free, the stream rides RTP; otherwise MJPEG over the
                // media channel, exactly as v1. The viewer side starts no
                // capture — it claims the inbound lane so arriving samples
                // route to its console window.
                if from_node == me && to_node != me {
                    let mode = self.pick_outbound_video_mode(route, &to_node);
                    // Which monitor: the synthetic `screen` is the primary;
                    // a `screen:<id>` capability names one of the others
                    // (the ids come from this machine's own monitor
                    // enumeration — see `video::extra_screens`).
                    let monitor = device_of(route.from.as_str())
                        .and_then(|dev| dev.strip_prefix("screen:").map(str::to_string))
                        .and_then(|id| id.parse::<u32>().ok());
                    tracing::info!(
                        "route {} active — streaming this {} to {} ({})",
                        route.id,
                        match monitor {
                            Some(id) => format!("monitor {id}"),
                            None => "screen".to_string(),
                        },
                        short_id(&to_node),
                        mode_label(mode),
                    );
                    self.start_video_stream(route, &to_node, mode, VideoSource::Screen(monitor));
                } else if to_node == me {
                    tracing::info!(
                        "route {} active — expecting screen frames from {}",
                        route.id,
                        short_id(&from_node)
                    );
                }
            }
            MediaKind::Video => {
                // A camera route — same stream, different lens: the source
                // capability names one of this machine's scanned cameras,
                // and its frames ride exactly the pipeline a screen does
                // (transport negotiation, lanes, tuning, status reports
                // included). The viewer side claims the inbound lane and
                // renders in whichever window watches the route — a
                // console's camera tab, a room's tile.
                if from_node == me && to_node != me {
                    let mode = self.pick_outbound_video_mode(route, &to_node);
                    let device = device_of(route.from.as_str()).unwrap_or_default();
                    tracing::info!(
                        "route {} active — streaming camera {device} to {} ({})",
                        route.id,
                        short_id(&to_node),
                        mode_label(mode),
                    );
                    self.start_video_stream(route, &to_node, mode, VideoSource::Camera(device));
                } else if to_node == me {
                    tracing::info!(
                        "route {} active — expecting camera frames from {}",
                        route.id,
                        short_id(&from_node)
                    );
                }
            }
            MediaKind::Input => {
                // The sink injects lazily per inbound event (behind the
                // ownership gate); the source is driven by the console
                // window via `send_input`. Nothing to start eagerly — but
                // say the link is live, so "awaiting accept" is never the
                // last word on a working control route.
                if from_node == me {
                    tracing::info!(
                        "route {} active — keyboard/mouse control to {}",
                        route.id,
                        short_id(&to_node)
                    );
                } else if to_node == me {
                    tracing::info!(
                        "route {} active — accepting control from {}",
                        route.id,
                        short_id(&from_node)
                    );
                }
            }
            MediaKind::Clipboard => {
                // Nothing to start eagerly: the source reads + streams its
                // clipboard per paste (`clipboard_paste`), and the sink
                // reassembles + writes it on arrival (`handle_clipboard_frame`).
                // Say the link is live so "awaiting accept" isn't the last
                // word on a working clipboard route.
                if from_node == me {
                    tracing::info!(
                        "route {} active — clipboard to {}",
                        route.id,
                        short_id(&to_node)
                    );
                } else if to_node == me {
                    tracing::info!(
                        "route {} active — accepting clipboard from {}",
                        route.id,
                        short_id(&from_node)
                    );
                }
            }
            MediaKind::Generic if is_terminal_route(route) => {
                if from_node == me && to_node == me {
                    // Loopback: a terminal to the machine we're sitting at.
                    // We're both shell *and* viewer — there's no peer to
                    // negotiate frames with, so the PTY's output goes
                    // straight into the local viewer queue (the same one the
                    // remote path enqueues into), and the window drains it
                    // exactly as it would a remote session.
                    self.start_terminal_loopback(route);
                } else if from_node == me && to_node != me {
                    // We're the shell end: spawn a PTY and pump it to the
                    // viewer (after re-clearing the owner/fleet gate).
                    self.start_terminal_host(route);
                } else if to_node == me && from_node != me {
                    // We're the viewer: buffer output from the very first
                    // byte — the host's prompt arrives right after Accept,
                    // before the terminal window has subscribed, and unlike
                    // a video frame a dropped byte never heals.
                    self.terminal.ensure_queue(&route.id);
                    tracing::info!(
                        "route {} active — terminal session from {}",
                        route.id,
                        short_id(&from_node)
                    );
                }
            }
            MediaKind::Generic if is_files_route(route) => {
                if from_node == me && to_node != me {
                    // We're the disk end: requests drive everything — the
                    // owner/fleet gate re-clears per inbound frame.
                    tracing::info!(
                        "route {} active — hosting files for {}",
                        route.id,
                        short_id(&to_node)
                    );
                } else if to_node == me && from_node != me {
                    // We're the viewer: buffer responses from the first
                    // frame, before the files window has subscribed.
                    self.files.ensure_queue(&route.id);
                    tracing::info!(
                        "route {} active — files session from {}",
                        route.id,
                        short_id(&from_node)
                    );
                }
            }
            MediaKind::Generic if is_shared_route(route) => {
                // A room's Shared Files fetch lane — the files plumbing,
                // but token-gated instead of owner/fleet (see
                // `handle_file_frame`). Downloads stream straight to disk
                // via the registered sink, so the viewer side just needs a
                // buffer for any reply that beats the registration.
                if from_node == me && to_node != me {
                    tracing::info!(
                        "route {} active — serving shared files to {}",
                        route.id,
                        short_id(&to_node)
                    );
                } else if to_node == me && from_node != me {
                    self.files.ensure_queue(&route.id);
                    tracing::info!(
                        "route {} active — shared-files fetch from {}",
                        route.id,
                        short_id(&from_node)
                    );
                }
            }
            MediaKind::Generic if is_site_route(route) => {
                // Nothing to start eagerly. The *client* (sink) already bound
                // its local listener at `site_map` time and opens tunnels as
                // connections arrive; the *host* (source) reacts to each
                // `SiteEvent::Open` (re-checking its own exposed allow-list)
                // in `handle_site_frame`. Just confirm the link is live.
                if from_node == me && to_node != me {
                    tracing::info!(
                        "route {} active — hosting site for {}",
                        route.id,
                        short_id(&to_node)
                    );
                } else if to_node == me && from_node != me {
                    tracing::info!(
                        "route {} active — site proxy from {}",
                        route.id,
                        short_id(&from_node)
                    );
                }
            }
            other => {
                tracing::info!(
                    "route {} active ({other:?}); media transport for it is a follow-up",
                    route.id
                );
            }
        }
    }

    /// The ids of the active-codec media routes between us and `peer` in one
    /// direction, sorted — the shared, signalling-free basis for lane
    /// assignment: both ends compute the identical list from their own copy of
    /// the session, so a route lands on the same lane on both. `codec` is
    /// "h264" (video) or "opus" (audio); `outbound` = we are the source.
    fn sorted_media_routes(&self, peer: &str, outbound: bool, codec: &str) -> Vec<String> {
        let Some(me) = self.local_node_id() else {
            return Vec::new();
        };
        let mp = pubkey_part(&me).to_string();
        let pc = pubkey_part(peer).to_string();
        let st = self.state.lock();
        let Some(session) = st.session.as_ref() else {
            return Vec::new();
        };
        let mut ids: Vec<String> = session
            .routes()
            .filter(|r| {
                let codecs = if codec == "opus" { &r.audio } else { &r.video };
                codecs.iter().any(|c| c == codec) && {
                    let src = pubkey_part(node_of(r.route.from.as_str()).as_str()).to_string();
                    let dst = pubkey_part(node_of(r.route.to.as_str()).as_str()).to_string();
                    if outbound {
                        src == mp && dst == pc
                    } else {
                        src == pc && dst == mp
                    }
                }
            })
            .map(|r| r.route.id.clone())
            .collect();
        ids.sort_unstable();
        ids
    }

    /// The media-lane pool size we and `peer` can both use for video: 0 when the
    /// local daemon has no track lane at all (everything MJPEG), 1 when either
    /// side predates the lane pool (only lane 0; extra streams fall back to
    /// MJPEG — the original behaviour), else the local pool size (both ends ship
    /// the same pinned daemon, so the counts match).
    fn effective_video_lanes(&self, peer: &str) -> u8 {
        if !self.daemon_video.load(Ordering::SeqCst) {
            return 0;
        }
        if self.peer_supports_lanes(peer) {
            self.daemon_lanes.load(Ordering::SeqCst).max(1)
        } else {
            1
        }
    }

    /// The audio twin of [`Self::effective_video_lanes`], gated on the audio lane.
    fn effective_audio_lanes(&self, peer: &str) -> u8 {
        if !self.daemon_audio.load(Ordering::SeqCst) {
            return 0;
        }
        if self.peer_supports_lanes(peer) {
            self.daemon_lanes.load(Ordering::SeqCst).max(1)
        } else {
            1
        }
    }

    /// Whether `peer` advertised the media-lane pool in its presence features.
    fn peer_supports_lanes(&self, peer: &str) -> bool {
        let canon = pubkey_part(peer);
        self.state.lock().peer_features.get(canon).is_some_and(|f| {
            f.iter()
                .any(|x| x == allmystuff_protocol::FEATURE_MEDIA_LANES)
        })
    }

    /// The lane an outbound H.264 video route to `peer` rides, or `None` for
    /// MJPEG (no usable lane, or the route's sorted position is past the
    /// effective pool). `outbound` = we are the source.
    fn video_lane(&self, route_id: &str, peer: &str, outbound: bool) -> Option<u8> {
        let cap = self.effective_video_lanes(peer);
        if cap == 0 {
            return None;
        }
        let idx = self
            .sorted_media_routes(peer, outbound, "h264")
            .iter()
            .position(|id| id == route_id)?;
        (idx < cap as usize).then_some(idx as u8)
    }

    /// The audio twin of [`Self::video_lane`] (Opus on the audio lane).
    fn audio_lane(&self, route_id: &str, peer: &str, outbound: bool) -> Option<u8> {
        let cap = self.effective_audio_lanes(peer);
        if cap == 0 {
            return None;
        }
        let idx = self
            .sorted_media_routes(peer, outbound, "opus")
            .iter()
            .position(|id| id == route_id)?;
        (idx < cap as usize).then_some(idx as u8)
    }

    /// The route whose inbound video samples arrive on `lane` from `peer` — the
    /// inverse of [`Self::video_lane`]: the lane-th H.264 video route from the
    /// peer to us in sorted order. Routes a `video_inbound` event to the right
    /// console window.
    fn video_route_for_lane(&self, peer: &str, lane: u8) -> Option<String> {
        self.sorted_media_routes(peer, false, "h264")
            .into_iter()
            .nth(lane as usize)
    }

    /// The audio twin of [`Self::video_route_for_lane`].
    fn audio_route_for_lane(&self, peer: &str, lane: u8) -> Option<String> {
        self.sorted_media_routes(peer, false, "opus")
            .into_iter()
            .nth(lane as usize)
    }

    /// The transport for a stream this machine is about to send on
    /// `route` — shared by the display and camera arms of
    /// [`Self::start_media`]: H.264 on the peer's track lane when the
    /// offer asked for it and the route's sorted position falls inside
    /// the effective lane pool; MJPEG over the media channel otherwise,
    /// exactly as v1.
    fn pick_outbound_video_mode(&self, route: &Route, to_node: &str) -> VideoMode {
        let accepts_h264 = self
            .state
            .lock()
            .session
            .as_ref()
            .and_then(|s| s.route(&route.id))
            .map(|r| r.video.iter().any(|v| v == "h264"))
            .unwrap_or(false);
        let daemon_video = self.daemon_video.load(Ordering::SeqCst);
        if accepts_h264 && !daemon_video {
            tracing::warn!(
                "route {} — viewer accepts H.264 but the local daemon predates the track lane (needs myownmesh ≥ 0.2.1); streaming MJPEG",
                route.id
            );
        }
        if accepts_h264 && self.video_lane(&route.id, to_node, true).is_some() {
            VideoMode::H264
        } else {
            VideoMode::Mjpeg
        }
    }

    /// Start the capture behind an outbound display/camera stream, wired
    /// to the packet forwarder and the in-band capture-status reports.
    fn start_video_stream(
        self: &Arc<Self>,
        route: &Route,
        to_node: &str,
        mode: VideoMode,
        source: VideoSource,
    ) {
        let peer = to_node.to_string();
        let tx = self.video_out.clone();
        let status_mesh = Arc::downgrade(self);
        let status_peer = peer.clone();
        let status_route = route.id.clone();
        let route_id = route.id.clone();
        self.video.start_capture(
            route.id.clone(),
            mode,
            source,
            move |packet| {
                // try_send: a full queue drops this packet; the next
                // capture carries a fresher picture.
                tx.try_send((peer.clone(), route_id.clone(), packet))
                    .is_ok()
            },
            move |state, detail| {
                // Capture-state transitions travel to the viewer in-band
                // (`vstat`), so its console can explain a black stage
                // instead of just showing one.
                let Some(mesh) = status_mesh.upgrade() else {
                    return;
                };
                let frame = VideoStatusFrame::new(status_route.clone(), state, detail);
                let peer = status_peer.clone();
                crate::spawn(async move {
                    let Ok(payload) = serde_json::to_value(&frame) else {
                        return;
                    };
                    if let Err(e) = mesh.send_media_value(&peer, payload).await {
                        tracing::debug!("capture status to {} failed: {e}", short_id(&peer));
                    }
                });
            },
        );
    }

    /// The host side of a terminal route going active: spawn this user's
    /// shell and pump its output to the viewer. The owner/fleet gate
    /// already ran at offer time ([`terminal_offer_refusal`]); it's
    /// re-checked here — and on every inbound byte — so a session can
    /// never outlive the authorization that allowed it.
    fn start_terminal_host(self: &Arc<Self>, route: &Route) {
        let viewer = node_of(route.to.as_str());
        let peer = self.route_peer(&route.id).unwrap_or(viewer);
        let rid = route.id.clone();
        if !self.sender_may_control(&peer) {
            tracing::warn!(
                "route {rid} — terminal for non-controller {} refused",
                short_id(&peer)
            );
            let mesh = self.clone();
            crate::spawn(async move {
                let _ = mesh.disconnect(rid).await;
            });
            return;
        }
        // One pump per viewer route. A duplicate `StartMedia` for this route
        // — the offer arriving on more than one shared network, say — must
        // not spawn a second pump onto it: two pumps fan the one shell's
        // output out twice (doubled/tripled terminal). The first start wins;
        // later duplicates are ignored until the pump ends and releases.
        if !self.term_pumps.lock().insert(rid.clone()) {
            tracing::debug!(
                "route {rid} — terminal pump already running; ignoring duplicate start"
            );
            return;
        }
        // The session the viewer asked to attach to: `Some(id)` joins that
        // shared shell (tmux-style — scrollback replayed, keyboard shared),
        // `None` mints a fresh one. The default emulator size is 80×24; the
        // viewer's first resize reconciles the shared PTY to its real size.
        let requested = self.requested_term_session(&route.id);
        match self
            .terminal
            .open(requested.as_deref(), &rid, TERM_INIT_COLS, TERM_INIT_ROWS)
        {
            Ok(attach) => {
                let session_id = attach.session_id.clone();
                tracing::info!(
                    "route {rid} active — {} terminal session {session_id} for {} ({} now attached)",
                    if attach.created { "hosting new" } else { "attaching to" },
                    short_id(&peer),
                    self.terminal
                        .list_sessions()
                        .iter()
                        .find(|s| s.session_id == session_id)
                        .map(|s| s.attachers)
                        .unwrap_or(1),
                );
                // Record the resolved id on our (host) route and echo it to
                // the viewer on a follow-up Accept, so its UI learns which
                // shell this is (and how to re-attach). Best-effort: the
                // first Accept already started the viewer's media.
                self.record_and_announce_term_session(&route.id, &peer, &session_id);
                let mesh = self.clone();
                crate::spawn(async move {
                    mesh.clone()
                        .pump_term_attach(rid.clone(), peer, attach)
                        .await;
                    // The pump ended (viewer detached, shell exited) — release
                    // the route so a genuine fresh start can pump again.
                    mesh.term_pumps.lock().remove(&rid);
                });
            }
            Err(e) => {
                // The shell never opened — release the route we just claimed.
                self.term_pumps.lock().remove(&rid);
                // Tell the viewer in its own terms — a terminal renders a
                // line of text better than a silently vanished route — then
                // tear the route down.
                tracing::warn!("route {rid} — shell didn't start: {e}");
                let mesh = self.clone();
                crate::spawn(async move {
                    let note = format!("[couldn't start a shell here: {e}]\r\n");
                    for frame in [
                        TermFrame::new(
                            &rid,
                            0,
                            TermEvent::Data {
                                bytes: note.into_bytes(),
                            },
                        ),
                        TermFrame::new(&rid, 1, TermEvent::Exit { code: None }),
                    ] {
                        if let Ok(payload) = serde_json::to_value(&frame) {
                            let _ = mesh.send_media_value(&peer, payload).await;
                        }
                    }
                    let _ = mesh.disconnect(rid).await;
                });
            }
        }
    }

    /// The terminal session this route asked to attach to, from the session
    /// snapshot — `Some(id)` for an explicit attach, `None` for "new shell".
    fn requested_term_session(&self, route_id: &str) -> Option<String> {
        self.state
            .lock()
            .session
            .as_ref()
            .and_then(|s| s.route(route_id))
            .and_then(|r| r.term_session.clone())
    }

    /// Record the resolved terminal session id on this (host) route, then
    /// echo it to the viewer with a follow-up `Accept` so its UI learns the
    /// shared id (for "shared with N" and re-attach). The first Accept the
    /// session auto-sent already started the viewer's media; this one only
    /// carries the resolved id.
    fn record_and_announce_term_session(
        self: &Arc<Self>,
        route_id: &str,
        peer: &str,
        session: &str,
    ) {
        {
            let mut st = self.state.lock();
            if let Some(s) = st.session.as_mut() {
                s.set_term_session(route_id, session.to_string());
            }
        }
        self.emit_snapshot();
        let mesh = self.clone();
        let peer = peer.to_string();
        let route_id = route_id.to_string();
        let session = session.to_string();
        crate::spawn(async move {
            let _ = mesh
                .send_control(
                    &peer,
                    &ControlMessage::Route(RouteControl::Accept {
                        route_id,
                        session: Some(session),
                    }),
                )
                .await;
        });
    }

    /// Pump one attacher's view of a shared terminal session to its viewer:
    /// replay the scrollback first (a fresh attach paints the current
    /// screen), then forward the session's live broadcast — this attacher's
    /// own pump to its own viewer route, so several viewers on one session
    /// each get the output (and, via `term_send`→`terminal.write`, each type
    /// into the one shell). `Lagged` skips ahead (output is live media);
    /// `Closed`/`Exit` ends *this* viewer's pump only.
    async fn pump_term_attach(
        self: Arc<Self>,
        rid: String,
        peer: String,
        attach: crate::terminal::TermAttach,
    ) {
        use tokio::sync::broadcast::error::RecvError;
        let crate::terminal::TermAttach {
            scrollback, mut rx, ..
        } = attach;
        let mut seq: u64 = 0;
        let mut last_ok = std::time::Instant::now();
        let mut last_warn = std::time::Instant::now() - WARN_EVERY;

        // Replay the current screen to *this* viewer before the live stream.
        if !scrollback.is_empty() {
            for frame in TermFrame::data_frames(&rid, seq, &scrollback, MAX_TERM_DATA_BYTES) {
                seq = frame.seq + 1;
                if let Ok(payload) = serde_json::to_value(&frame) {
                    let _ = self.send_media_value(&peer, payload).await;
                }
            }
        }

        loop {
            let msg = match rx.recv().await {
                Ok(msg) => msg,
                // A slow attacher fell behind the broadcast ring — output is
                // live media, so skip ahead rather than wedge the shell.
                Err(RecvError::Lagged(n)) => {
                    tracing::debug!("terminal {rid} — viewer lagged {n} chunks; skipping ahead");
                    continue;
                }
                // The session ended (shell exited / closed) — end this pump.
                Err(RecvError::Closed) => return,
            };
            // This viewer detached (closed its tab, or its route was torn
            // down) — stop pumping to it. The shell lives on for the other
            // attachers; the last one leaving arms the idle reaper. Checked
            // here so a closed viewer's pump never keeps streaming to a dead
            // route.
            if !self.terminal.is_attached(&rid) {
                return;
            }
            match msg {
                OutMsg::Data(bytes) => {
                    for frame in TermFrame::data_frames(&rid, seq, &bytes, MAX_TERM_DATA_BYTES) {
                        seq = frame.seq + 1;
                        let Ok(payload) = serde_json::to_value(&frame) else {
                            continue;
                        };
                        match self.send_media_value(&peer, payload).await {
                            Ok(()) => last_ok = std::time::Instant::now(),
                            Err(e) => {
                                if last_warn.elapsed() >= WARN_EVERY {
                                    last_warn = std::time::Instant::now();
                                    tracing::warn!(
                                        "terminal output to {} failed: {e}",
                                        short_id(&peer)
                                    );
                                }
                                // Nothing else reaps a session whose viewer
                                // silently vanished (peer drops never reach
                                // the session) — the pump is the watchdog.
                                // Detach this viewer only; the shell lives on
                                // for the other attachers (or a re-attach
                                // that replays scrollback), never killed
                                // because one viewer's link blipped.
                                if last_ok.elapsed() > TERM_SEND_PATIENCE {
                                    tracing::warn!(
                                        "terminal {rid} — viewer unreachable; detaching (shell kept for reattach)"
                                    );
                                    self.terminal.detach(&rid);
                                    return;
                                }
                            }
                        }
                    }
                }
                OutMsg::Resize { cols, rows } => {
                    // The shared PTY's authoritative size changed — tell this
                    // viewer so it renders (letterboxes) to the one shell's
                    // size and its wrapping matches everyone else's.
                    let frame = TermFrame::new(&rid, seq, TermEvent::Resize { cols, rows });
                    seq += 1;
                    if let Ok(payload) = serde_json::to_value(&frame) {
                        let _ = self.send_media_value(&peer, payload).await;
                    }
                }
                OutMsg::Exit(code) => {
                    tracing::info!("terminal {rid} — shell ended ({code:?})");
                    let frame = TermFrame::new(&rid, seq, TermEvent::Exit { code });
                    if let Ok(payload) = serde_json::to_value(&frame) {
                        let _ = self.send_media_value(&peer, payload).await;
                    }
                    // The shell ended for *everyone* on this session — tear
                    // this viewer's route down. Other attachers' pumps see
                    // the same `Exit`/`Closed` and end on their own.
                    let _ = self.disconnect(rid.clone()).await;
                    return;
                }
            }
        }
    }

    /// A **loopback** terminal route going active: a terminal to the very
    /// machine we're sitting at, where this node is both shell *and* viewer.
    /// There's no peer, so instead of framing the PTY's output onto the mesh
    /// we feed it straight into the local viewer queue (the same one the
    /// remote viewer path enqueues into) and poke the window — the Terminal
    /// UI can't tell a loopback session from a remote one. Keystrokes and
    /// resizes from the window short-circuit to `terminal.write/resize`
    /// locally (see [`Self::term_send`]). The owner/fleet gate is re-cleared
    /// for consistency with the remote host path — it's our own machine, so
    /// it passes.
    fn start_terminal_loopback(self: &Arc<Self>, route: &Route) {
        let rid = route.id.clone();
        // The peer here is ourselves; the gate must still pass (owner or a
        // fleet member always controls their own machine), and re-running it
        // keeps the loopback path honest with the remote one.
        let peer = self
            .route_peer(&rid)
            .unwrap_or_else(|| node_of(route.to.as_str()));
        if !self.sender_may_control(&peer) {
            tracing::warn!(
                "route {rid} — local terminal refused (not owner/fleet of this machine)"
            );
            let mesh = self.clone();
            crate::spawn(async move {
                let _ = mesh.disconnect(rid).await;
            });
            return;
        }
        // One pump per route, exactly as the remote host path: a duplicate
        // local `StartMedia` must not spawn a second loopback pump onto this
        // route (which would double the window's output). First start wins.
        if !self.term_pumps.lock().insert(rid.clone()) {
            tracing::debug!(
                "route {rid} — local terminal pump already running; ignoring duplicate"
            );
            return;
        }
        // Buffer output from the very first byte — the shell's prompt is
        // produced right after Accept, before the window has subscribed, and
        // a dropped terminal byte never heals.
        self.terminal.ensure_queue(&rid);
        // The session this local window asked to attach to: `Some(id)` lets
        // two local windows share one local shell (multi-attach to yourself),
        // `None` mints a fresh one — the same session model as the remote
        // host path, just feeding the local queue instead of the mesh.
        let requested = self.requested_term_session(&rid);
        match self
            .terminal
            .open(requested.as_deref(), &rid, TERM_INIT_COLS, TERM_INIT_ROWS)
        {
            Ok(attach) => {
                let session_id = attach.session_id.clone();
                tracing::info!(
                    "route {rid} active — local terminal session {session_id} ({})",
                    if attach.created {
                        "new shell"
                    } else {
                        "attached"
                    },
                );
                // Record the resolved id locally so a snapshot surfaces it
                // (the loopback UI shows the same "shared with N" line); there
                // is no peer to Accept back to.
                {
                    let mut st = self.state.lock();
                    if let Some(s) = st.session.as_mut() {
                        s.set_term_session(&rid, session_id.clone());
                    }
                }
                self.emit_snapshot();
                let crate::terminal::TermAttach {
                    scrollback, mut rx, ..
                } = attach;
                // Replay the current screen into this window's queue first
                // (an attach to an already-running local shell paints it),
                // then pump the shared broadcast in.
                if !scrollback.is_empty() && self.terminal.enqueue(&rid, scrollback) {
                    self.sink.emit("allmystuff://term-ready", json!(rid));
                }
                let mesh = self.clone();
                crate::spawn(async move {
                    use tokio::sync::broadcast::error::RecvError;
                    loop {
                        let msg = match rx.recv().await {
                            Ok(msg) => msg,
                            Err(RecvError::Lagged(_)) => continue,
                            Err(RecvError::Closed) => break,
                        };
                        match msg {
                            OutMsg::Data(bytes) => {
                                // Straight into the local viewer queue. A
                                // queue going empty → non-empty is the cue to
                                // poke the window, exactly as the inbound
                                // remote viewer path does.
                                if mesh.terminal.enqueue(&rid, bytes) {
                                    mesh.sink.emit("allmystuff://term-ready", json!(rid));
                                }
                            }
                            OutMsg::Resize { cols, rows } => {
                                // Two local windows sharing one shell: tell this
                                // window the shared size so it letterboxes to it.
                                mesh.sink.emit(
                                    "allmystuff://term-resize",
                                    json!({ "route": rid, "cols": cols, "rows": rows }),
                                );
                            }
                            OutMsg::Exit(code) => {
                                tracing::info!("local terminal {rid} — shell ended ({code:?})");
                                mesh.sink.emit(
                                    "allmystuff://term-exit",
                                    json!({ "route": rid, "code": code }),
                                );
                                let _ = mesh.disconnect(rid.clone()).await;
                                break;
                            }
                        }
                    }
                    // Pump ended — release the route so a fresh start can pump.
                    mesh.term_pumps.lock().remove(&rid);
                });
            }
            Err(e) => {
                // The shell never opened — release the route we just claimed.
                self.term_pumps.lock().remove(&rid);
                // Render the failure to the window in its own terms — a line
                // of text, then the exit — then tear the route down.
                tracing::warn!("route {rid} — local shell didn't start: {e}");
                let note = format!("[couldn't start a shell here: {e}]\r\n");
                if self.terminal.enqueue(&rid, note.into_bytes()) {
                    self.sink.emit("allmystuff://term-ready", json!(rid));
                }
                self.sink.emit(
                    "allmystuff://term-exit",
                    json!({ "route": rid, "code": serde_json::Value::Null }),
                );
                let mesh = self.clone();
                crate::spawn(async move {
                    let _ = mesh.disconnect(rid).await;
                });
            }
        }
    }

    /// Whether a terminal frame on `route` is fresh (record its seq and take
    /// it) or a duplicate to drop — used both for output the viewer takes
    /// (`term_rx_seq`) and input the host takes (`term_in_seq`). Each sending
    /// side numbers a route's frames strictly increasing, so any seq at or
    /// below the last we took is the same send arriving again over another
    /// shared network (control and media ride them all). A forward jump (the
    /// sender skipped ahead after a broadcast lag) is still fresh.
    fn accept_term_seq(seqs: &Mutex<HashMap<String, u64>>, route: &str, seq: u64) -> bool {
        let mut seqs = seqs.lock();
        match seqs.get(route) {
            Some(&last) if seq <= last => false,
            _ => {
                seqs.insert(route.to_string(), seq);
                true
            }
        }
    }

    /// One inbound terminal frame. Which side we are comes from the route
    /// itself: keystrokes/resizes landing on the *host* (the route sources
    /// here) clear the same two gates as input injection — live route from
    /// this exact sender, and the sender being an authorized controller;
    /// output/exit landing on the *viewer* (the route sinks here) goes to
    /// the watching terminal window.
    fn handle_term_frame(&self, from: &str, frame: TermFrame) {
        let Some(me) = self.local_node_id() else {
            return;
        };
        let (hosts_here, views_here) = {
            let st = self.state.lock();
            let Some(r) = st.session.as_ref().and_then(|s| s.route(&frame.route)) else {
                return;
            };
            if !(r.is_active()
                && is_terminal_route(&r.route)
                && pubkey_part(r.peer.as_str()) == pubkey_part(from))
            {
                tracing::debug!(
                    "terminal frame for {} refused (route not live here)",
                    frame.route
                );
                return;
            }
            (
                node_of(r.route.from.as_str()) == me,
                node_of(r.route.to.as_str()) == me,
            )
        };
        if hosts_here {
            if !self.sender_may_control(from) {
                tracing::warn!("dropped terminal input from {from}: not an authorized controller");
                return;
            }
            // Drop a duplicate keystroke/resize: the viewer numbers its
            // outbound frames strictly increasing, so a seq we've already
            // applied is the same send redelivered on another shared network.
            // Without this the PTY is written N times and the shell echoes
            // `aaaa` for one keypress.
            if !Self::accept_term_seq(&self.term_in_seq, &frame.route, frame.seq) {
                return;
            }
            match frame.event {
                TermEvent::Data { bytes } => {
                    let _ = self.terminal.write(&frame.route, bytes);
                }
                TermEvent::Resize { cols, rows } => {
                    let _ = self.terminal.resize(&frame.route, cols, rows);
                }
                // Ending the shell is the host's report, never the
                // viewer's request — a viewer ends a session by tearing
                // the route down.
                TermEvent::Exit { .. } => {}
                // A terminal event a newer viewer introduced — ignore it.
                TermEvent::Unknown => {}
            }
        } else if views_here {
            // Drop a duplicate delivery (see `accept_term_seq`): the same send
            // arriving again over another shared network. Without this the
            // window paints every byte — and the shell appears to echo every
            // keystroke — once per shared network: the doubled/tripled terminal.
            if !Self::accept_term_seq(&self.term_rx_seq, &frame.route, frame.seq) {
                return;
            }
            match frame.event {
                TermEvent::Data { bytes } => {
                    if self.terminal.enqueue(&frame.route, bytes) {
                        // Queue went empty → non-empty: poke the window to
                        // drain (a lost poke costs latency, never bytes —
                        // the safety poll catches up).
                        self.sink
                            .emit("allmystuff://term-ready", json!(frame.route));
                    }
                }
                TermEvent::Exit { code } => {
                    self.sink.emit(
                        "allmystuff://term-exit",
                        json!({ "route": frame.route, "code": code }),
                    );
                }
                TermEvent::Resize { cols, rows } => {
                    // The host's authoritative shared size — the window renders
                    // (letterboxes) to it so its wrapping matches the one shell.
                    self.sink.emit(
                        "allmystuff://term-resize",
                        json!({ "route": frame.route, "cols": cols, "rows": rows }),
                    );
                }
                // A terminal event a newer host introduced — ignore it.
                TermEvent::Unknown => {}
            }
        }
    }

    /// Front-end command: keystrokes/resizes from a terminal window down
    /// its active terminal route. This machine must be the route's
    /// *viewer* (its sink side); `Exit` is the host's word and is refused.
    pub async fn term_send(
        self: &Arc<Self>,
        route_id: String,
        event: TermEvent,
    ) -> Result<(), String> {
        let me = self.local_node_id().ok_or("mesh not ready")?;
        let (peer, loopback) = {
            let st = self.state.lock();
            let r = st
                .session
                .as_ref()
                .and_then(|s| s.route(&route_id))
                .ok_or("unknown route")?;
            if !(r.is_active() && is_terminal_route(&r.route) && node_of(r.route.to.as_str()) == me)
            {
                return Err("route isn't an active terminal session here".into());
            }
            // A terminal to this very machine has no peer to frame to: the
            // shell is hosted right here, so input/resize go straight to the
            // local PTY rather than out over the mesh.
            let loopback = node_of(r.route.from.as_str()) == me;
            (r.peer.to_string(), loopback)
        };
        if loopback {
            match event {
                TermEvent::Data { bytes } => {
                    let _ = self.terminal.write(&route_id, bytes);
                    return Ok(());
                }
                TermEvent::Resize { cols, rows } => {
                    let _ = self.terminal.resize(&route_id, cols, rows);
                    return Ok(());
                }
                TermEvent::Exit { .. } => {
                    return Err("exit is reported by the host, not sent".into())
                }
                TermEvent::Unknown => return Err("unknown terminal event".into()),
            }
        }
        match event {
            TermEvent::Data { bytes } => {
                // A paste can be arbitrarily large: chunk to the channel
                // budget and await each send, so big pastes throttle
                // themselves instead of flooding the daemon.
                let frames = TermFrame::data_frames(&route_id, 0, &bytes, MAX_TERM_DATA_BYTES);
                let first = self
                    .term_seq
                    .fetch_add(frames.len() as u64, Ordering::Relaxed);
                for (i, mut frame) in frames.into_iter().enumerate() {
                    frame.seq = first + i as u64;
                    let payload = serde_json::to_value(&frame).map_err(|e| e.to_string())?;
                    self.send_media_value(&peer, payload).await?;
                }
                Ok(())
            }
            TermEvent::Resize { .. } => {
                let seq = self.term_seq.fetch_add(1, Ordering::Relaxed);
                let frame = TermFrame::new(&route_id, seq, event);
                let payload = serde_json::to_value(&frame).map_err(|e| e.to_string())?;
                self.send_media_value(&peer, payload).await
            }
            TermEvent::Exit { .. } => Err("exit is reported by the host, not sent".into()),
            // We never originate an `Unknown` event; reject it for exhaustiveness.
            TermEvent::Unknown => Err("unknown terminal event".into()),
        }
    }

    /// A terminal window claims an active route's buffered output (returns
    /// the token scoping its unwatch). Pure plumbing to [`TerminalHost`].
    pub fn term_watch(&self, route_id: &str) -> u64 {
        self.terminal.watch_output(route_id)
    }

    pub fn term_unwatch(&self, route_id: &str, token: u64) {
        self.terminal.unwatch(route_id, token);
    }

    /// Drain buffered terminal output (`[u32 le len][bytes]…`), emptied by
    /// the window on each `allmystuff://term-ready` poke or safety poll.
    pub fn term_poll(&self, route_id: &str) -> Vec<u8> {
        self.terminal.poll(route_id)
    }

    /// Front-end command: ask `node` for its open terminal sessions so the
    /// picker can offer to *attach* to one (multi-attach) instead of always
    /// minting a new shell. For a remote machine this fires a
    /// [`RouteControl::TerminalSessionsRequest`]; the host's answer arrives
    /// asynchronously as an `allmystuff://terminal-sessions` event. For the
    /// **local** machine there's no peer to ask — we answer at once from our
    /// own [`TerminalHost`], returning the list directly (and `None` for a
    /// remote ask, whose reply rides the event). Gated owner/fleet exactly
    /// like opening a terminal — the host re-checks it too.
    pub async fn request_terminal_sessions(
        self: &Arc<Self>,
        node: String,
    ) -> Result<Option<Vec<TerminalSessionInfo>>, String> {
        let me = self.local_node_id().ok_or("mesh not ready")?;
        if pubkey_part(&node) == pubkey_part(&me) {
            // Our own shells — answer straight from the local host.
            return Ok(Some(self.terminal_session_infos()));
        }
        self.send_control(
            &node,
            &ControlMessage::Route(RouteControl::TerminalSessionsRequest),
        )
        .await?;
        Ok(None)
    }

    /// The local terminal host's open sessions in the protocol's wire shape.
    fn terminal_session_infos(&self) -> Vec<TerminalSessionInfo> {
        self.terminal
            .list_sessions()
            .into_iter()
            .map(|s| TerminalSessionInfo {
                session_id: s.session_id,
                title: s.title,
                created_unix: s.created_unix,
                attachers: s.attachers,
            })
            .collect()
    }

    /// Answer a viewer's [`RouteControl::TerminalSessionsRequest`]: reply on
    /// the control channel with this host's open terminal sessions — gated by
    /// the same owner/fleet check the terminal host itself uses, so a
    /// stranger on the mesh can't even enumerate our shells.
    async fn handle_terminal_sessions_request(self: &Arc<Self>, from: &str) {
        if !self.sender_may_control(from) {
            tracing::warn!(
                "terminal-sessions request from {} ignored: not owner/fleet",
                short_id(from)
            );
            return;
        }
        let sessions = self.terminal_session_infos();
        let _ = self
            .send_control(
                from,
                &ControlMessage::Route(RouteControl::TerminalSessions { sessions }),
            )
            .await;
    }

    /// One inbound file frame. Which side we are comes from the route
    /// itself: requests landing on the *host* (the route sources here)
    /// clear the same two gates as terminal input — live route from this
    /// exact sender, and the sender being an authorized controller;
    /// responses landing on the *viewer* (the route sinks here) go to the
    /// watching files window — except chunks of a registered download,
    /// which stream straight to disk.
    fn handle_file_frame(self: &Arc<Self>, from: &str, frame: FileFrame) {
        let Some(me) = self.local_node_id() else {
            return;
        };
        let (hosts_here, views_here, shared) = {
            let st = self.state.lock();
            let Some(r) = st.session.as_ref().and_then(|s| s.route(&frame.route)) else {
                return;
            };
            let shared = is_shared_route(&r.route);
            if !(r.is_active()
                && (is_files_route(&r.route) || shared)
                && pubkey_part(r.peer.as_str()) == pubkey_part(from))
            {
                tracing::debug!(
                    "file frame for {} refused (route not live here)",
                    frame.route
                );
                return;
            }
            (
                node_of(r.route.from.as_str()) == me,
                node_of(r.route.to.as_str()) == me,
                shared,
            )
        };
        if hosts_here && shared {
            // A Shared Files lane: token-gated, never owner/fleet, and only
            // ever a `Fetch` — no path browsing, no writes. The token's
            // allow-list (the room's members, as the uploader stated them)
            // is the gate, re-cleared per request.
            match &frame.event {
                FileEvent::Fetch { req, token } => match self.shared_path_for(token, from) {
                    Some(path) => self.start_files_request(
                        &frame.route,
                        from,
                        FileEvent::Read { req: *req, path },
                    ),
                    None => {
                        tracing::warn!(
                            "dropped shared-file fetch from {}: token not shared with them",
                            short_id(from)
                        );
                        self.send_file_event(
                            frame.route.clone(),
                            from.to_string(),
                            FileEvent::Err {
                                req: *req,
                                reason: "that file isn't shared with you (or no longer is)".into(),
                            },
                        );
                    }
                },
                // A `:shared` route carries nothing else from the viewer.
                other => tracing::debug!("shared-files host ignoring {other:?}"),
            }
        } else if hosts_here {
            if !self.sender_may_control(from) {
                tracing::warn!("dropped file request from {from}: not an authorized controller");
                return;
            }
            match &frame.event {
                // Upload pieces are applied inline: pieces of one upload
                // must land in arrival order (the viewer sends them
                // sequentially), and a piece is one small append.
                FileEvent::Write { .. } => {
                    if let Some(reply) = crate::files::write_piece(&frame.event) {
                        self.send_file_event(frame.route.clone(), from.to_string(), reply);
                    }
                }
                FileEvent::List { .. }
                | FileEvent::Read { .. }
                | FileEvent::Mkdir { .. }
                | FileEvent::Rename { .. }
                | FileEvent::Delete { .. } => {
                    self.start_files_request(&frame.route, from, frame.event);
                }
                // Response kinds (and `Fetch`, which only a `:shared` route
                // serves) landing on the files host are a confused peer.
                _ => {}
            }
        } else if views_here {
            // A chunk of a registered download streams to disk, not to
            // the window; everything else is queued for the window.
            if let FileEvent::Chunk { req, .. } = &frame.event {
                if self.feed_download(&frame.route, *req, &frame.event) {
                    return;
                }
            }
            if let FileEvent::Err { req, .. } = &frame.event {
                // A failed request that had a download registered: close
                // and discard the partial file, then let the window see
                // the error too.
                self.fail_download(&frame.route, *req, &frame.event);
            }
            let Ok(bytes) = serde_json::to_vec(&frame) else {
                return;
            };
            if self.files.enqueue(&frame.route, bytes) {
                self.sink
                    .emit("allmystuff://file-ready", json!(frame.route));
            }
        }
    }

    /// Host side: run one request against the local filesystem and pump
    /// its response events back to the viewer. A send failure aborts the
    /// pump (dropping the receiver cancels the op at its next chunk) —
    /// unlike a shell, a request/response op is simply retried by the
    /// viewer.
    fn start_files_request(self: &Arc<Self>, route_id: &str, peer: &str, event: FileEvent) {
        let mut rx = self.files.handle(route_id, event);
        let mesh = self.clone();
        let rid = route_id.to_string();
        let peer = peer.to_string();
        crate::spawn(async move {
            while let Some(ev) = rx.recv().await {
                let seq = mesh.file_seq.fetch_add(1, Ordering::Relaxed);
                let frame = FileFrame::new(&rid, seq, ev);
                let Ok(payload) = serde_json::to_value(&frame) else {
                    continue;
                };
                if let Err(e) = mesh.send_media_value(&peer, payload).await {
                    tracing::warn!("file response to {} failed: {e}", short_id(&peer));
                    return; // dropping rx cancels the op
                }
            }
        });
    }

    /// Send one host-side file event (an upload piece's reply) to the
    /// viewer, fire-and-forget.
    fn send_file_event(self: &Arc<Self>, route_id: String, peer: String, event: FileEvent) {
        let mesh = self.clone();
        crate::spawn(async move {
            let seq = mesh.file_seq.fetch_add(1, Ordering::Relaxed);
            let frame = FileFrame::new(&route_id, seq, event);
            if let Ok(payload) = serde_json::to_value(&frame) {
                if let Err(e) = mesh.send_media_value(&peer, payload).await {
                    tracing::warn!("file reply to {} failed: {e}", short_id(&peer));
                }
            }
        });
    }

    /// Front-end command: one file *request* from a files window down its
    /// active files route. This machine must be the route's *viewer* (its
    /// sink side); response kinds are the host's word and are refused.
    pub async fn file_send(
        self: &Arc<Self>,
        route_id: String,
        event: FileEvent,
    ) -> Result<(), String> {
        // A `Fetch` rides a `:shared` route (the Shared Files area); every
        // other request rides a `:files` route (the file manager). Pairing
        // the event to its route keeps a shared lane fetch-only.
        let want_shared = matches!(event, FileEvent::Fetch { .. });
        match event {
            FileEvent::List { .. }
            | FileEvent::Read { .. }
            | FileEvent::Fetch { .. }
            | FileEvent::Write { .. }
            | FileEvent::Mkdir { .. }
            | FileEvent::Rename { .. }
            | FileEvent::Delete { .. } => {}
            _ => return Err("responses come from the host, not the viewer".into()),
        }
        let me = self.local_node_id().ok_or("mesh not ready")?;
        let peer = {
            let st = self.state.lock();
            let r = st
                .session
                .as_ref()
                .and_then(|s| s.route(&route_id))
                .ok_or("unknown route")?;
            let kind_ok = if want_shared {
                is_shared_route(&r.route)
            } else {
                is_files_route(&r.route)
            };
            if !(r.is_active() && kind_ok && node_of(r.route.to.as_str()) == me) {
                return Err("route isn't an active files session here".into());
            }
            r.peer.to_string()
        };
        let seq = self.file_seq.fetch_add(1, Ordering::Relaxed);
        let frame = FileFrame::new(&route_id, seq, event);
        let payload = serde_json::to_value(&frame).map_err(|e| e.to_string())?;
        self.send_media_value(&peer, payload).await
    }

    // ---- sites (the reverse proxy) --------------------------------------

    /// This machine's discovered listening services (the full set, so the
    /// UI can offer each to expose). The active banner probe runs here, off
    /// the presence-build path, with a short per-port timeout.
    pub fn site_scan(&self) -> Vec<allmystuff_inventory::ListeningService> {
        let mut listening = allmystuff_inventory::scan().listening;
        allmystuff_inventory::listening::probe_services(
            &mut listening,
            std::time::Duration::from_millis(200),
        );
        // Diagnostic: which listening ports the scan found (set
        // ALLMYSTUFF_GUI_LOG=info to see it). "0 found" on a box that's
        // clearly serving means the platform probe came up empty.
        tracing::info!(
            "site scan found {} listening service(s): {}",
            listening.len(),
            listening
                .iter()
                .map(|s| format!(":{}", s.port))
                .collect::<Vec<_>>()
                .join(" ")
        );
        listening
    }

    /// The services this machine currently advertises, as id → display name.
    pub fn site_exposed(&self) -> std::collections::BTreeMap<String, String> {
        self.sites.exposed_map()
    }

    /// Set the exposed set (id → display name) and re-stamp presence so peers
    /// see the change.
    pub async fn site_set_exposed(
        self: &Arc<Self>,
        exposed: std::collections::BTreeMap<String, String>,
    ) -> std::collections::BTreeMap<String, String> {
        let map = self.sites.set_exposed(exposed);
        // Rebuild + re-advertise this node's profile (its `sites` follow the
        // exposed set). Re-broadcast so peers' Sites tabs update promptly.
        self.restamp_profile().await;
        map
    }

    /// Every site this device currently has mapped: `(node, host_port,
    /// local_port)`.
    pub fn site_mappings(&self) -> Vec<(String, u16, u16)> {
        self.sites.list_mappings()
    }

    // ---- remote site management (a fleet device's drawer) -------------

    /// Ask a co-owned machine for its full site list (to manage its exposure
    /// from its drawer). The reply lands as the `allmystuff://node-sites`
    /// event. Fire-and-forget; the far side gates on owner/fleet.
    pub async fn site_remote_list(self: &Arc<Self>, node: String) -> Result<(), String> {
        self.send_control(&node, &ControlMessage::Site(SiteControl::List))
            .await
    }

    /// Tell a co-owned machine to advertise exactly `exposed` (id → name).
    /// The far side gates on owner/fleet, applies it, and re-advertises.
    pub async fn site_remote_set_exposed(
        self: &Arc<Self>,
        node: String,
        exposed: std::collections::BTreeMap<String, String>,
    ) -> Result<(), String> {
        self.send_control(
            &node,
            &ControlMessage::Site(SiteControl::SetExposed { exposed }),
        )
        .await
    }

    /// One inbound site-management control message. `List` / `SetExposed` are
    /// privileged (they read or change what this machine exposes), so only an
    /// owner/fleet sender is answered — the same gate as the proxy itself.
    /// `Sites` is a reply we surface to the front-end.
    async fn handle_site_control(self: &Arc<Self>, from: &str, sc: SiteControl) {
        match sc {
            SiteControl::List => {
                if !self.sender_may_control(from) {
                    tracing::warn!("site list from {} refused: not owner/fleet", short_id(from));
                    return;
                }
                // Scan + probe is blocking, so do it off the event loop, then
                // reply to the asking machine.
                let mesh = self.clone();
                let peer = from.to_string();
                crate::spawn(async move {
                    let scan = mesh.clone();
                    let Ok((services, exposed)) = tokio::task::spawn_blocking(move || {
                        let services = scan
                            .site_scan()
                            .into_iter()
                            .map(|s| SiteService {
                                id: s.id,
                                name: s.name,
                                port: s.port,
                                scheme: s.scheme,
                                loopback: s.loopback,
                                process: s.process,
                                title: s.title,
                            })
                            .collect::<Vec<_>>();
                        (services, scan.sites.exposed_map())
                    })
                    .await
                    else {
                        return;
                    };
                    let _ = mesh
                        .send_control(
                            &peer,
                            &ControlMessage::Site(SiteControl::Sites { services, exposed }),
                        )
                        .await;
                });
            }
            SiteControl::Sites { services, exposed } => {
                // A managed machine's answer — hand it to the drawer.
                self.sink.emit(
                    "allmystuff://node-sites",
                    serde_json::json!({ "from": from, "services": services, "exposed": exposed }),
                );
            }
            SiteControl::SetExposed { exposed } => {
                if !self.sender_may_control(from) {
                    tracing::warn!(
                        "site set-exposed from {} refused: not owner/fleet",
                        short_id(from)
                    );
                    return;
                }
                self.sites.set_exposed(exposed);
                self.restamp_profile().await;
            }
            // A site-management kind a newer build introduced — ignore it.
            SiteControl::Unknown => {}
        }
    }

    /// Map a peer's site to a local port: bind a local listener (direct port
    /// when free, else remapped), offer the reverse-proxy route, and start
    /// the accept loop. Returns the bound local port. The far side gates the
    /// offer owner/fleet and re-checks every connection's port against its
    /// own exposed allow-list.
    pub async fn site_map(self: &Arc<Self>, node: String, port: u16) -> Result<u16, String> {
        let me = self.local_node_id().ok_or("mesh not ready")?;
        if pubkey_part(&node) == pubkey_part(&me) {
            return Err("that's this device".into());
        }
        // Already mapped? Hand back the existing local port (idempotent).
        if let Some((_, _, lp)) = self
            .sites
            .list_mappings()
            .into_iter()
            .find(|(n, hp, _)| pubkey_part(n) == pubkey_part(&node) && *hp == port)
        {
            return Ok(lp);
        }
        // Bind a local listener, preferring the same port number, then a free
        // one — the OS is the final arbiter, so retry on a lost race.
        let (listener, local_port) = self.bind_site_listener(port).await?;
        // Mint the route: generic media, source `<host>:site`, sink a
        // per-mapping viewer endpoint (never a catalog capability — shape is
        // the contract, like terminal/files).
        let seq = self.site_seq.fetch_add(1, Ordering::Relaxed);
        let from = format!("{node}:site");
        let to = format!("{me}:site-view:{}-{seq}", port);
        let route_id = format!("route:{from}→{to}");
        // Offer the route through the session (drives offer→accept→active).
        let msg = {
            let mut st = self.state.lock();
            let s = st.session.as_mut().ok_or("mesh not ready")?;
            let route = Route {
                id: route_id.clone(),
                from: from.clone().into(),
                to: to.clone().into(),
                media: MediaKind::Generic,
            };
            s.offer(route, node.as_str(), Vec::new(), Vec::new())
        };
        if let Err(e) = self.send_control(&node, &msg).await {
            let mut st = self.state.lock();
            if let Some(s) = st.session.as_mut() {
                let _ = s.teardown(&route_id);
            }
            return Err(e);
        }
        // Start accepting local connections; each becomes one tunneled conn.
        let accept = self.spawn_site_accept(route_id.clone(), node.clone(), port, listener);
        self.sites
            .add_mapping(route_id, ClientMapping::new(node, port, local_port, accept));
        Ok(local_port)
    }

    /// Unmap a site: tear the route down (closing the listener + every
    /// connection via `StopMedia`) and tell the host.
    pub async fn site_unmap(self: &Arc<Self>, node: String, port: u16) -> Result<(), String> {
        let Some(route_id) = self.sites.route_for(&node, port) else {
            return Ok(()); // nothing mapped — idempotent
        };
        self.disconnect(route_id).await
    }

    /// Bind a local TCP listener for a site, preferring the host's port
    /// number ("direct"), falling back to a remapped high port, and finally
    /// to an OS-assigned one — so a mapping always lands somewhere.
    async fn bind_site_listener(
        &self,
        host_port: u16,
    ) -> Result<(tokio::net::TcpListener, u16), String> {
        use std::net::{Ipv4Addr, SocketAddr};
        let taken = self.sites.taken_local_ports();
        let preferred = allmystuff_bridge::sites::allocate_local_port(host_port, &taken);
        // Bind loopback only — a mapped site is for *this* machine's clients,
        // never re-exposed onto this machine's LAN.
        for candidate in [preferred, 0] {
            let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, candidate));
            if let Ok(listener) = tokio::net::TcpListener::bind(addr).await {
                let port = listener
                    .local_addr()
                    .map(|a| a.port())
                    .map_err(|e| e.to_string())?;
                return Ok((listener, port));
            }
        }
        Err(format!(
            "couldn't bind a local port for the site on :{host_port}"
        ))
    }

    /// Client side: accept local connections on `listener` and tunnel each
    /// over `route_id`. One mesh route multiplexes every connection by a
    /// client-minted `conn` id.
    fn spawn_site_accept(
        self: &Arc<Self>,
        route_id: String,
        peer: String,
        host_port: u16,
        listener: tokio::net::TcpListener,
    ) -> tokio::task::JoinHandle<()> {
        let mesh = self.clone();
        crate::spawn(async move {
            // Wait for the host to accept before taking connections — until
            // the route is active a tunnel's `Open` would be dropped, leaving
            // a connecting client hung. (Pending TCP connections sit in the
            // OS backlog meanwhile.) If the host rejects or never answers, we
            // give up and the listener closes with this task.
            if !mesh.await_route_active(&route_id).await {
                tracing::warn!("site route {route_id} never went active — not accepting");
                return;
            }
            let mut next_conn: u64 = 0;
            loop {
                let (socket, _addr) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(e) => {
                        tracing::debug!("site listener {route_id} stopped: {e}");
                        return;
                    }
                };
                next_conn += 1;
                let conn = next_conn;
                // Register the channel before wiring, then tunnel (the
                // client sends `Open` so the host dials loopback). Over the
                // per-route cap → refuse this one cleanly.
                match mesh.sites.open_conn(&route_id, conn) {
                    Some(rx) => mesh.wire_conn(&route_id, &peer, conn, socket, rx, Some(host_port)),
                    None => {
                        mesh.send_site_event(&peer, &route_id, SiteEvent::Close { conn })
                            .await;
                        tracing::warn!(
                            "site route {route_id} at connection cap — refused conn {conn}"
                        );
                    }
                }
            }
        })
    }

    /// Poll until a route is active (it just went through offer→accept), or
    /// give up after ~5s — the client's accept loop gate, so it never opens a
    /// tunnel the host isn't ready for (and bails cleanly if the host
    /// rejected the offer). Returns whether it became active.
    async fn await_route_active(&self, route_id: &str) -> bool {
        for _ in 0..100 {
            let active = {
                let st = self.state.lock();
                st.session
                    .as_ref()
                    .and_then(|s| s.route(route_id))
                    .map(|r| r.is_active())
            };
            match active {
                Some(true) => return true,
                None => return false, // route gone (torn down / never made)
                Some(false) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
            }
        }
        false
    }

    /// Wire one tunneled connection whose inbound channel is already
    /// registered (via `open_conn`, so `rx` is its receiver): split the
    /// socket, spawn the inbound writer and the socket→mesh reader, and
    /// attach the reader. Read and write run as independent tasks — full
    /// duplex — so a WebSocket-upgraded (or otherwise long-lived,
    /// bidirectional) connection flows both ways for its whole life. When
    /// `open_port` is set (the client side), a `SiteEvent::Open` goes first
    /// so the host dials loopback. Shared by both ends.
    fn wire_conn(
        self: &Arc<Self>,
        route_id: &str,
        peer: &str,
        conn: u64,
        socket: tokio::net::TcpStream,
        rx: mpsc::Receiver<Vec<u8>>,
        open_port: Option<u16>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut read_half, mut write_half) = socket.into_split();

        // Inbound bytes (from the peer) → this connection's socket. Detached:
        // it ends when `tx` is dropped (close_conn / teardown), then shuts
        // the write half so the local client sees a clean close. It drains
        // any bytes that were buffered before the socket was wired.
        let mut rx = rx;
        crate::spawn(async move {
            while let Some(bytes) = rx.recv().await {
                if write_half.write_all(&bytes).await.is_err() {
                    break;
                }
            }
            let _ = write_half.shutdown().await;
        });

        // Socket bytes → the peer, as `SiteEvent::Data` frames (backpressured
        // by the mesh send — a slow link parks this read, never drops bytes).
        // On EOF a `Close`, then close_conn (dropping `tx` stops the writer).
        let mesh = self.clone();
        let rid = route_id.to_string();
        let peer_s = peer.to_string();
        let reader = crate::spawn(async move {
            if let Some(port) = open_port {
                mesh.send_site_event(&peer_s, &rid, SiteEvent::Open { conn, port })
                    .await;
            }
            let mut buf = vec![0u8; SITE_CHUNK_BYTES];
            loop {
                match read_half.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        mesh.send_site_event(
                            &peer_s,
                            &rid,
                            SiteEvent::Data {
                                conn,
                                data: buf[..n].to_vec(),
                            },
                        )
                        .await;
                    }
                }
            }
            mesh.send_site_event(&peer_s, &rid, SiteEvent::Close { conn })
                .await;
            mesh.sites.close_conn(&rid, conn);
        });

        self.sites.attach_reader(route_id, conn, reader);
    }

    /// Send one `SiteEvent` to `peer` on the media channel, fire-and-forget
    /// (a send failure is logged; the route's teardown handles the rest).
    async fn send_site_event(self: &Arc<Self>, peer: &str, route_id: &str, event: SiteEvent) {
        let seq = self.site_seq.fetch_add(1, Ordering::Relaxed);
        let frame = SiteFrame::new(route_id, seq, event);
        if let Ok(payload) = serde_json::to_value(&frame) {
            if let Err(e) = self.send_media_value(peer, payload).await {
                tracing::debug!("site frame to {} failed: {e}", short_id(peer));
            }
        }
    }

    /// One inbound site frame. Which side we are comes from the route: a
    /// frame for a route that *sources* here lands on the host (it dials
    /// loopback); one that *sinks* here lands on the client (it writes to a
    /// local socket). Either way the route must be live, a site route, and
    /// from this exact peer; the host additionally re-checks the sender is an
    /// authorized controller and the requested port is one *it* advertises.
    fn handle_site_frame(self: &Arc<Self>, from: &str, frame: SiteFrame) {
        let Some(me) = self.local_node_id() else {
            return;
        };
        let (hosts_here, views_here) = {
            let st = self.state.lock();
            let Some(r) = st.session.as_ref().and_then(|s| s.route(&frame.route)) else {
                return;
            };
            if !(r.is_active()
                && is_site_route(&r.route)
                && pubkey_part(r.peer.as_str()) == pubkey_part(from))
            {
                tracing::debug!(
                    "site frame for {} refused (route not live here)",
                    frame.route
                );
                return;
            }
            (
                node_of(r.route.from.as_str()) == me,
                node_of(r.route.to.as_str()) == me,
            )
        };

        if hosts_here {
            // The proxy *into* this machine — as privileged as the terminal,
            // so the same owner/fleet gate, re-cleared per frame.
            if !self.sender_may_control(from) {
                tracing::warn!(
                    "dropped site frame from {}: not an authorized controller",
                    short_id(from)
                );
                return;
            }
            match frame.event {
                SiteEvent::Open { conn, port } => {
                    // The load-bearing control: dial only a port *we* expose,
                    // never the client's free choice. Over the per-route cap,
                    // or unexposed → refuse with a `Close`.
                    let rx = if self.sites.is_port_exposed(port) {
                        self.sites.open_conn(&frame.route, conn)
                    } else {
                        tracing::warn!(
                            "site open from {} for :{port} refused — not an exposed service",
                            short_id(from)
                        );
                        None
                    };
                    match rx {
                        Some(rx) => self.spawn_site_host_connect(
                            frame.route.clone(),
                            from.to_string(),
                            conn,
                            port,
                            rx,
                        ),
                        None => {
                            let mesh = self.clone();
                            let (route, peer) = (frame.route.clone(), from.to_string());
                            crate::spawn(async move {
                                mesh.send_site_event(&peer, &route, SiteEvent::Close { conn })
                                    .await;
                            });
                        }
                    }
                }
                SiteEvent::Data { conn, data } => self.feed_site_conn(&frame.route, conn, data),
                SiteEvent::Close { conn } => self.sites.close_conn(&frame.route, conn),
                // A site event a newer client introduced — ignore it.
                SiteEvent::Unknown => {}
            }
        } else if views_here {
            // The client end — the host's bytes for one of our mapped
            // connections. We never receive `Open` here (we mint those).
            match frame.event {
                SiteEvent::Data { conn, data } => self.feed_site_conn(&frame.route, conn, data),
                SiteEvent::Close { conn } => self.sites.close_conn(&frame.route, conn),
                SiteEvent::Open { conn, .. } => {
                    tracing::debug!("ignoring unexpected site Open {conn} on the client side");
                }
                // A site event a newer host introduced — ignore it.
                SiteEvent::Unknown => {}
            }
        }
    }

    /// Deliver inbound bytes to a connection's local socket. Non-blocking:
    /// if the socket is too backed up to take more (its queue is full), the
    /// connection is *reset* rather than dropping bytes or growing unbounded
    /// — a TCP client just reconnects.
    fn feed_site_conn(self: &Arc<Self>, route_id: &str, conn: u64, data: Vec<u8>) {
        let Some(tx) = self.sites.conn_tx(route_id, conn) else {
            return; // unknown/closed connection
        };
        if tx.try_send(data).is_err() {
            self.sites.close_conn(route_id, conn);
        }
    }

    /// Host side: a validated `Open` whose channel is already registered
    /// (`rx` is its receiver). Connect to the local service and wire the
    /// tunnel; inbound `Data` that arrived during the connect sits buffered
    /// in `rx` and is drained once the writer starts. A failed connect closes
    /// the connection back to the client (and drops its registration).
    fn spawn_site_host_connect(
        self: &Arc<Self>,
        route_id: String,
        peer: String,
        conn: u64,
        port: u16,
        rx: mpsc::Receiver<Vec<u8>>,
    ) {
        use std::net::{Ipv4Addr, SocketAddr};
        let mesh = self.clone();
        crate::spawn(async move {
            let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
            match tokio::net::TcpStream::connect(addr).await {
                Ok(socket) => {
                    // The host doesn't send Open (the client already did).
                    mesh.wire_conn(&route_id, &peer, conn, socket, rx, None);
                }
                Err(e) => {
                    tracing::warn!("site connect to 127.0.0.1:{port} failed: {e}");
                    mesh.sites.close_conn(&route_id, conn);
                    mesh.send_site_event(&peer, &route_id, SiteEvent::Close { conn })
                        .await;
                }
            }
        });
    }

    // ---- Shared Files (the call's "Shared Files" area) ------------------

    /// Offer files into a room's Shared Files area. Each readable path gets
    /// an opaque fetch token, registered with the set of members allowed to
    /// pull it (`members`, canonical node ids). Returns one
    /// [`SharedFileMeta`] per file that could be read — the GUI hands these
    /// to the room's host, which restates them in the room's list. The
    /// bytes stay here; only the token + name + size travel.
    pub fn room_share_files(
        &self,
        members: Vec<String>,
        paths: Vec<String>,
    ) -> Vec<SharedFileMeta> {
        let allowed: std::collections::HashSet<String> =
            members.iter().map(|m| pubkey_part(m).to_string()).collect();
        let mut out = Vec::new();
        let mut reg = self.shared.lock();
        for path in paths {
            let p = std::path::PathBuf::from(&path);
            let Ok(meta) = std::fs::metadata(&p) else {
                tracing::warn!("can't share {path}: not readable");
                continue;
            };
            if meta.is_dir() {
                tracing::warn!("can't share {path}: it's a folder");
                continue;
            }
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".to_string());
            let token = fresh_share_token();
            reg.insert(
                token.clone(),
                SharedReg {
                    path: p,
                    allowed: allowed.clone(),
                },
            );
            out.push(SharedFileMeta {
                token,
                name,
                size: meta.len(),
            });
        }
        out
    }

    /// Refresh the members allowed to fetch a set of shared tokens — the
    /// room's roster changed (a join, an admit, a removal) while these
    /// files were on offer. Unknown tokens are skipped.
    pub fn room_set_share_peers(&self, tokens: Vec<String>, members: Vec<String>) {
        let allowed: std::collections::HashSet<String> =
            members.iter().map(|m| pubkey_part(m).to_string()).collect();
        let mut reg = self.shared.lock();
        for t in tokens {
            if let Some(s) = reg.get_mut(&t) {
                s.allowed = allowed.clone();
            }
        }
    }

    /// Stop offering files (the uploader removed them, or left the room).
    pub fn room_unshare(&self, tokens: Vec<String>) {
        let mut reg = self.shared.lock();
        for t in tokens {
            reg.remove(&t);
        }
    }

    /// Resolve a fetch token to its on-disk path, but only for a peer it
    /// was shared with — the Shared Files gate. `None` when the token is
    /// unknown or `from` isn't on its allow-list.
    fn shared_path_for(&self, token: &str, from: &str) -> Option<String> {
        let reg = self.shared.lock();
        let s = reg.get(token)?;
        if !s.allowed.contains(pubkey_part(from)) {
            return None;
        }
        Some(s.path.to_string_lossy().into_owned())
    }

    /// A files window claims an active route's buffered responses (returns
    /// the token scoping its unwatch). Pure plumbing to [`FilesPlane`].
    pub fn file_watch(&self, route_id: &str) -> u64 {
        self.files.watch(route_id)
    }

    pub fn file_unwatch(&self, route_id: &str, token: u64) {
        self.files.unwatch(route_id, token);
    }

    /// Drain buffered file responses (`[u32 le len][frame json]…`), emptied
    /// by the window on each `allmystuff://file-ready` poke or safety poll.
    pub fn file_poll(&self, route_id: &str) -> Vec<u8> {
        self.files.poll(route_id)
    }

    /// Register a download sink: the `Chunk`s of `(route_id, req)` stream
    /// into this machine's Downloads folder under `name` (unique-ified)
    /// instead of the window's queue. Called *before* the Read request is
    /// sent, so the first chunk can't race the registration. Returns the
    /// destination path.
    pub fn file_download(&self, route_id: String, req: u64, name: &str) -> Result<String, String> {
        // The name comes from the remote listing — keep only its final
        // component so it can't steer the write outside Downloads.
        let base = std::path::Path::new(name)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| !n.is_empty() && n != "." && n != "..")
            .unwrap_or_else(|| "download".to_string());
        let dir = dirs::download_dir()
            .or_else(dirs::home_dir)
            .ok_or("no Downloads folder here")?;
        let path = unique_path(&dir, &base);
        let file = std::fs::File::create(&path).map_err(|e| e.to_string())?;
        self.downloads.lock().insert(
            (route_id, req),
            DownloadSink {
                file,
                path: path.clone(),
                written: 0,
                last_progress: std::time::Instant::now(),
            },
        );
        Ok(path.to_string_lossy().into_owned())
    }

    /// Stream one chunk into its registered download, if any. Returns
    /// `true` when the chunk was consumed here (don't queue it). Finishing
    /// (or failing) emits `allmystuff://file-saved` so the window can say
    /// where it landed.
    fn feed_download(&self, route_id: &str, req: u64, event: &FileEvent) -> bool {
        use std::io::Write as _;
        let FileEvent::Chunk {
            data, total, eof, ..
        } = event
        else {
            return false;
        };
        let key = (route_id.to_string(), req);
        let mut map = self.downloads.lock();
        let Some(sink) = map.get_mut(&key) else {
            return false;
        };
        if let Err(e) = sink.file.write_all(data) {
            let path = sink.path.clone();
            map.remove(&key);
            drop(map);
            let _ = std::fs::remove_file(&path);
            self.sink.emit(
                "allmystuff://file-saved",
                json!({ "route": route_id, "req": req, "path": null, "error": e.to_string() }),
            );
            return true;
        }
        sink.written += data.len() as u64;
        if *eof {
            let Some(sink) = map.remove(&key) else {
                return true;
            };
            drop(map);
            let _ = sink.file.sync_all();
            self.sink.emit(
                "allmystuff://file-saved",
                json!({
                    "route": route_id, "req": req,
                    "path": sink.path.to_string_lossy(), "error": null,
                }),
            );
        } else if sink.last_progress.elapsed() >= std::time::Duration::from_millis(250) {
            sink.last_progress = std::time::Instant::now();
            let written = sink.written;
            drop(map);
            self.sink.emit(
                "allmystuff://file-progress",
                json!({ "route": route_id, "req": req, "written": written, "total": total }),
            );
        }
        true
    }

    /// The host answered a registered download with an error: discard the
    /// partial file and tell the window.
    fn fail_download(&self, route_id: &str, req: u64, event: &FileEvent) {
        let FileEvent::Err { reason, .. } = event else {
            return;
        };
        let key = (route_id.to_string(), req);
        let Some(sink) = self.downloads.lock().remove(&key) else {
            return;
        };
        let _ = std::fs::remove_file(&sink.path);
        self.sink.emit(
            "allmystuff://file-saved",
            json!({ "route": route_id, "req": req, "path": null, "error": reason }),
        );
    }

    /// Discard every download sink a route had (it ended) — partial files
    /// are deleted, never left half-written in Downloads.
    fn drop_downloads(&self, route_id: &str) {
        let mut map = self.downloads.lock();
        let keys: Vec<_> = map
            .keys()
            .filter(|(rid, _)| rid == route_id)
            .cloned()
            .collect();
        for key in keys {
            if let Some(sink) = map.remove(&key) {
                let _ = std::fs::remove_file(&sink.path);
            }
        }
    }

    /// Whether an inbound media frame is acceptable: its route is one this
    /// session knows, is live, carries `media`, sinks on this machine, and
    /// the daemon-authenticated sender is the route's peer.
    fn inbound_media_ok(&self, route_id: &str, sender: &str, media: MediaKind) -> bool {
        let Some(me) = self.local_node_id() else {
            return false;
        };
        let st = self.state.lock();
        let Some(r) = st.session.as_ref().and_then(|s| s.route(route_id)) else {
            return false;
        };
        r.is_active()
            && r.route.media == media
            && node_of(r.route.to.as_str()) == me
            && pubkey_part(r.peer.as_str()) == pubkey_part(sender)
    }

    /// [`Self::inbound_media_ok`] for the frame kinds two media share:
    /// video frames (and their `vstat` reports) belong to a display route
    /// *or* a camera one — same pipeline, different lens.
    fn inbound_video_ok(&self, route_id: &str, sender: &str) -> bool {
        self.inbound_media_ok(route_id, sender, MediaKind::Display)
            || self.inbound_media_ok(route_id, sender, MediaKind::Video)
    }

    /// Why an inbound video frame was refused, in one diagnosable line —
    /// which [`Self::inbound_media_ok`] condition failed, with the facts.
    fn route_diag(&self, route_id: &str, sender: &str) -> String {
        let me = self.local_node_id().unwrap_or_default();
        let st = self.state.lock();
        match st.session.as_ref().and_then(|s| s.route(route_id)) {
            None => "this session doesn't know the route".to_string(),
            Some(r) => format!(
                "route state {:?} · media {:?} · sinks here: {} · sender is its peer: {}",
                r.state,
                r.route.media,
                node_of(r.route.to.as_str()) == me,
                pubkey_part(r.peer.as_str()) == pubkey_part(sender),
            ),
        }
    }

    /// Rate limit for the inbound-video diagnostics: true at most once per
    /// [`WARN_EVERY`] per `key`, so a dead stream explains itself in the
    /// log without arriving at frame rate.
    fn diag_ok(&self, key: &str) -> bool {
        let mut map = self.video_diag_last.lock();
        let now = std::time::Instant::now();
        match map.get(key) {
            Some(t) if now.duration_since(*t) < WARN_EVERY => false,
            _ => {
                map.insert(key.to_string(), now);
                true
            }
        }
    }

    /// Whether `sender` may drive this device's privileged planes (terminal,
    /// files, input, sites, console). Trust comes from **authenticated**
    /// sources only: the recorded owner, or membership in the fleet's
    /// closed-network **signed roster** (cached in [`Mesh::fleet_authorized`]
    /// from the daemon). Nobody else — not even a peer a route auto-accepted
    /// for.
    ///
    /// No gossiped roster is consulted — the fleet has none any more. The old
    /// `CHANNEL_OWNED` `OwnedRoster` gossip was exactly the conscription vector
    /// this closes (AMS-01); membership is now the signed roster a peer can
    /// only enter via the owner's governance. Fails closed — an empty or stale
    /// cache denies control rather than guessing.
    ///
    /// Role model: a **member** is a node that can *be* controlled, a
    /// **manager** *can* control (and admit members), an **owner** controls and
    /// sets managers/owners. Today any signed-roster member may control any
    /// other (so managers reach managers, members, and even owners — as
    /// intended), and the per-role granularity that would let a *member* be
    /// controlled without itself initiating control is the deferred "more
    /// granular control" work — not mapped here yet.
    fn sender_may_control(&self, sender: &str) -> bool {
        let canon = pubkey_part(sender);
        if self.ownership.owner().as_deref().map(pubkey_part) == Some(canon) {
            return true;
        }
        self.fleet_authorized.lock().contains(canon)
    }

    /// Ask the far end of an inbound display/camera route for a clean
    /// decode entry (IDR) *now* — the decoder here lost its place.
    /// Rate-limited per route: decode errors arrive at frame rate, the
    /// asks must not.
    /// Old peers don't know the message and drop it; recovery then waits
    /// for the periodic IDR exactly as before.
    pub async fn request_refresh(self: &Arc<Self>, route_id: String) -> Result<(), String> {
        {
            let mut asks = self.refresh_asks.lock();
            let now = std::time::Instant::now();
            if asks
                .get(&route_id)
                .is_some_and(|t| now.duration_since(*t) < std::time::Duration::from_millis(600))
            {
                return Ok(());
            }
            asks.insert(route_id.clone(), now);
        }
        let peer = self.route_peer(&route_id).ok_or("unknown route")?;
        tracing::debug!("asking {} to re-key {route_id}", short_id(&peer));
        self.send_control(
            &peer,
            &ControlMessage::Route(RouteControl::Refresh { route_id }),
        )
        .await
    }

    /// Ask the far end of an inbound display/camera route to stream with
    /// these quality picks (`None` = that dial back on automatic). Old
    /// peers drop the message and stay on automatic.
    pub async fn request_tune(
        self: &Arc<Self>,
        route_id: String,
        max_edge: Option<u32>,
        bitrate: Option<u32>,
        fps: Option<u32>,
    ) -> Result<(), String> {
        let peer = self.route_peer(&route_id).ok_or("unknown route")?;
        // The streaming side logs the retune it actually applies — one
        // line per pill change across the pair is plenty.
        tracing::debug!(
            "asking {} to tune {route_id}: edge {max_edge:?} · bitrate {bitrate:?} · fps {fps:?}",
            short_id(&peer)
        );
        self.send_control(
            &peer,
            &ControlMessage::Route(RouteControl::Tune {
                route_id,
                max_edge,
                bitrate,
                fps,
            }),
        )
        .await
    }

    /// Report this viewer's decode health for an inbound route back to its
    /// streamer (receiver → sender), so the streamer can adapt the stream.
    /// Best-effort and unacknowledged: an old streamer drops the message and
    /// never adapts, exactly as today.
    pub async fn send_video_feedback(
        self: &Arc<Self>,
        route_id: String,
        recv_fps: u32,
        decode_fails: u32,
        queue_depth: u32,
    ) -> Result<(), String> {
        let peer = self.route_peer(&route_id).ok_or("unknown route")?;
        self.send_control(
            &peer,
            &ControlMessage::Route(RouteControl::VideoFeedback {
                route_id,
                recv_fps,
                decode_fails,
                queue_depth,
            }),
        )
        .await
    }

    /// Front-end command: forward one keyboard/mouse event down an active
    /// outbound input route (the console window's control stream).
    pub async fn send_input(
        self: &Arc<Self>,
        route_id: String,
        action: InputAction,
    ) -> Result<(), String> {
        let me = self.local_node_id().ok_or("mesh not ready")?;
        let peer = {
            let st = self.state.lock();
            let r = st
                .session
                .as_ref()
                .and_then(|s| s.route(&route_id))
                .ok_or("unknown route")?;
            if !(r.is_active()
                && r.route.media == MediaKind::Input
                && node_of(r.route.from.as_str()) == me)
            {
                return Err("route isn't an active outbound control link".into());
            }
            r.peer.to_string()
        };
        let seq = self.input_seq.fetch_add(1, Ordering::Relaxed);
        let ev = InputEvent::new(route_id, seq, action);
        let payload = serde_json::to_value(&ev).map_err(|e| e.to_string())?;
        self.send_media_value(&peer, payload).await
    }

    /// Front-end command: read this machine's clipboard and push it down an
    /// active outbound clipboard route — called the instant the console
    /// forwards a paste, so the far side pastes *our* content. Text rides one
    /// frame; an image or files ride a chunked transfer (the same shape the
    /// video/term/file planes use). This machine must be the route's source
    /// side; the far end gates the write the same way it gates input
    /// injection. The bytes are read here (the only place that can see file
    /// references on the OS clipboard).
    pub async fn clipboard_paste(self: &Arc<Self>, route_id: String) -> Result<(), String> {
        let me = self.local_node_id().ok_or("mesh not ready")?;
        let peer = {
            let st = self.state.lock();
            let r = st
                .session
                .as_ref()
                .and_then(|s| s.route(&route_id))
                .ok_or("unknown route")?;
            if !(r.is_active()
                && r.route.media == MediaKind::Clipboard
                && node_of(r.route.from.as_str()) == me)
            {
                return Err("route isn't an active outbound clipboard link".into());
            }
            r.peer.to_string()
        };
        self.send_clipboard_contents(&peer, &route_id).await
    }

    /// Front-end command: copy/cut **from** the remote — ask the far end to
    /// read its clipboard now and send it back on `route_id`, so the content
    /// it just copied lands on *this* machine. The mirror of
    /// [`Self::clipboard_paste`]: the console forwards the copy keystroke down
    /// the control route first (so the remote copies its selection into its
    /// own clipboard), then calls this. We mark the pull so the reply is let
    /// through ([`Self::handle_clipboard_frame`]) and fire the request. This
    /// machine must be the route's source side, exactly as for a paste.
    pub async fn clipboard_pull(self: &Arc<Self>, route_id: String) -> Result<(), String> {
        let me = self.local_node_id().ok_or("mesh not ready")?;
        let peer = {
            let st = self.state.lock();
            let r = st
                .session
                .as_ref()
                .and_then(|s| s.route(&route_id))
                .ok_or("unknown route")?;
            if !(r.is_active()
                && r.route.media == MediaKind::Clipboard
                && node_of(r.route.from.as_str()) == me)
            {
                return Err("route isn't an active outbound clipboard link".into());
            }
            r.peer.to_string()
        };
        // Open the acceptance window *before* the request goes out, so the
        // reply can never beat it (the remote replies on this same route).
        self.clip_pull_at
            .lock()
            .insert(route_id.clone(), std::time::Instant::now());
        self.send_clip_frame(&peer, &route_id, ClipboardEvent::Pull)
            .await
    }

    /// Read this machine's OS clipboard and stream it to `peer` on `route_id`
    /// — the shared body of [`Self::clipboard_paste`] (pushing our clipboard
    /// for the far side to paste) and the [`Pull`](ClipboardEvent::Pull)
    /// reply (sending our just-copied clipboard back to a controller). Text
    /// rides one frame; an image or files ride a chunked transfer, the same
    /// shape the video/term/file planes use.
    async fn send_clipboard_contents(
        self: &Arc<Self>,
        peer: &str,
        route_id: &str,
    ) -> Result<(), String> {
        // Read the OS clipboard off its dedicated thread (a blocking call).
        let svc = self.clipboard.clone();
        let clip = tokio::task::spawn_blocking(move || svc.read())
            .await
            .map_err(|e| e.to_string())?;
        let Some(clip) = clip else {
            return Ok(()); // empty / unreadable clipboard — nothing to send
        };
        match clip {
            LocalClip::Text(text) => {
                self.send_clip_frame(peer, route_id, ClipboardEvent::Text { text })
                    .await
            }
            LocalClip::Image(png) => {
                let transfer = self.clipboard_transfer.fetch_add(1, Ordering::Relaxed);
                let items = vec![ClipboardItem {
                    name: "image.png".into(),
                    size: png.len() as u64,
                }];
                self.send_clip_frame(
                    peer,
                    route_id,
                    ClipboardEvent::Open {
                        transfer,
                        content: ClipboardContentKind::Image,
                        items,
                    },
                )
                .await?;
                for piece in png.chunks(CLIPBOARD_CHUNK_BYTES) {
                    self.send_clip_frame(
                        peer,
                        route_id,
                        ClipboardEvent::Chunk {
                            transfer,
                            item: 0,
                            data: piece.to_vec(),
                        },
                    )
                    .await?;
                }
                self.send_clip_frame(peer, route_id, ClipboardEvent::Close { transfer })
                    .await
            }
            LocalClip::Files(files) => {
                let total: u64 = files.iter().map(|f| f.size).sum();
                if total > MAX_CLIPBOARD_BYTES {
                    return Err(format!(
                        "clipboard files are too large to paste across ({total} bytes)"
                    ));
                }
                let transfer = self.clipboard_transfer.fetch_add(1, Ordering::Relaxed);
                let items = files
                    .iter()
                    .map(|f| ClipboardItem {
                        name: f.name.clone(),
                        size: f.size,
                    })
                    .collect();
                self.send_clip_frame(
                    peer,
                    route_id,
                    ClipboardEvent::Open {
                        transfer,
                        content: ClipboardContentKind::Files,
                        items,
                    },
                )
                .await?;
                for (i, f) in files.iter().enumerate() {
                    // Stream each file from disk in channel-sized pieces, so a
                    // big paste never loads the whole file into memory.
                    let mut file = std::fs::File::open(&f.path).map_err(|e| e.to_string())?;
                    let mut buf = vec![0u8; CLIPBOARD_CHUNK_BYTES];
                    loop {
                        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
                        if n == 0 {
                            break;
                        }
                        self.send_clip_frame(
                            peer,
                            route_id,
                            ClipboardEvent::Chunk {
                                transfer,
                                item: i as u32,
                                data: buf[..n].to_vec(),
                            },
                        )
                        .await?;
                    }
                }
                self.send_clip_frame(peer, route_id, ClipboardEvent::Close { transfer })
                    .await
            }
        }
    }

    /// Send one clipboard frame to `peer` on `route_id`, fire-and-forget over
    /// the media channel (the same path control input rides).
    async fn send_clip_frame(
        &self,
        peer: &str,
        route_id: &str,
        event: ClipboardEvent,
    ) -> Result<(), String> {
        let seq = self.clipboard_seq.fetch_add(1, Ordering::Relaxed);
        let frame = ClipboardFrame::new(route_id, seq, event);
        let payload = serde_json::to_value(&frame).map_err(|e| e.to_string())?;
        self.send_media_value(peer, payload).await
    }

    /// A clipboard route carries frames both ways, like the files plane:
    ///   * **Sink side** (we're the route's `to`) — the controlled machine.
    ///     A paste pushes the controller's clipboard here, so we reassemble it
    ///     and write our OS clipboard; a [`Pull`](ClipboardEvent::Pull) asks
    ///     for *our* clipboard (a copy/cut driven from the console), so we read
    ///     it and stream it back. Either way it's part of being driven, so it
    ///     takes the same gate as input injection: a live route from this exact
    ///     sender *and* that sender being our owner or a co-owned fleet member.
    ///   * **Source side** (we're the route's `from`) — the controller. This
    ///     is the reply to a copy/cut we pulled, so we write it to our OS
    ///     clipboard — but only inside the window our own [`Self::clipboard_pull`]
    ///     opened, so a peer can never push onto our clipboard unasked.
    ///
    /// Text is one frame; an image or files arrive as a chunked transfer that
    /// commits on `Close`. A paste/copy keystroke rides the paired control
    /// route on the same ordered channel, so order is honoured end to end.
    fn handle_clipboard_frame(self: &Arc<Self>, from: &str, frame: ClipboardFrame) {
        let Some(me) = self.local_node_id() else {
            return;
        };
        let (sinks_here, sources_here) = {
            let st = self.state.lock();
            let Some(r) = st.session.as_ref().and_then(|s| s.route(&frame.route)) else {
                return;
            };
            if !(r.is_active()
                && r.route.media == MediaKind::Clipboard
                && pubkey_part(r.peer.as_str()) == pubkey_part(from))
            {
                return;
            }
            (
                node_of(r.route.to.as_str()) == me,
                node_of(r.route.from.as_str()) == me,
            )
        };

        if sinks_here {
            if !self.sender_may_control(from) {
                tracing::warn!("dropped clipboard from {from}: not an authorized controller");
                return;
            }
            if let ClipboardEvent::Pull = frame.event {
                // Copy/cut *from* this machine: the controller forwarded the
                // copy keystroke just ahead of this on the same ordered
                // channel, so our clipboard is (about to be) the freshly-copied
                // selection. Give the OS a beat to land it, then stream it back
                // on this route — the mirror of a paste. Through `crate::spawn`
                // (never a bare `tokio::spawn`), so it rides the engine's
                // registered runtime handle like every other engine task.
                let mesh = self.clone();
                let peer = from.to_string();
                let route = frame.route;
                crate::spawn(async move {
                    tokio::time::sleep(CLIPBOARD_COPY_SETTLE).await;
                    if let Err(e) = mesh.send_clipboard_contents(&peer, &route).await {
                        tracing::warn!("clipboard pull reply failed: {e}");
                    }
                });
                return;
            }
            self.apply_clipboard_event(frame.route, frame.event);
        } else if sources_here {
            // Accept a reply only inside the window our own pull opened; a
            // transfer's opening frame consumes that window (one reply per
            // pull), and its later Chunk/Close ride through on the
            // clip_inbound entry the Open registered (unknown transfers no-op).
            let accept = match &frame.event {
                ClipboardEvent::Text { .. } | ClipboardEvent::Open { .. } => self
                    .clip_pull_at
                    .lock()
                    .remove(&frame.route)
                    .is_some_and(|t| t.elapsed() < CLIPBOARD_PULL_WINDOW),
                ClipboardEvent::Chunk { .. } | ClipboardEvent::Close { .. } => true,
                ClipboardEvent::Pull | ClipboardEvent::Unknown => false,
            };
            if accept {
                self.apply_clipboard_event(frame.route, frame.event);
            }
        }
    }

    /// Write one received clipboard event to this machine's OS clipboard —
    /// the shared body of both directions of [`Self::handle_clipboard_frame`].
    /// Text commits at once; an image or files reassemble across a transfer
    /// and commit on `Close`. File bytes stream to a per-transfer staging dir
    /// the OS clipboard is then pointed at.
    fn apply_clipboard_event(&self, route: String, event: ClipboardEvent) {
        match event {
            ClipboardEvent::Text { text } => self.clipboard.set_text(text),
            ClipboardEvent::Open {
                transfer,
                content,
                items,
            } => {
                let total: u64 = items.iter().map(|i| i.size).sum();
                if total > MAX_CLIPBOARD_BYTES {
                    tracing::warn!("clipboard transfer too large ({total} bytes) — refused");
                    return;
                }
                if content == ClipboardContentKind::Files {
                    let dir = crate::clipboard::staging_dir(transfer);
                    let _ = std::fs::remove_dir_all(&dir);
                    if let Err(e) = std::fs::create_dir_all(&dir) {
                        tracing::warn!("clipboard staging dir failed: {e}");
                        return;
                    }
                }
                self.clip_inbound
                    .lock()
                    .insert((route, transfer), ClipInbound::new(content, items));
            }
            ClipboardEvent::Chunk {
                transfer,
                item,
                data,
            } => {
                let key = (route, transfer);
                let mut inbound = self.clip_inbound.lock();
                let Some(t) = inbound.get_mut(&key) else {
                    return; // unknown / already-dropped transfer
                };
                t.received += data.len() as u64;
                let over = t.received > MAX_CLIPBOARD_BYTES;
                if !over {
                    match t.content {
                        ClipboardContentKind::Image => t.image.extend_from_slice(&data),
                        ClipboardContentKind::Files => {
                            if let Some(name) = t.items.get(item as usize).map(|i| i.name.clone()) {
                                let first = !t.started[item as usize];
                                t.started[item as usize] = true;
                                let path =
                                    crate::clipboard::staging_dir(transfer).join(safe_name(&name));
                                if let Err(e) = append_chunk(&path, &data, first) {
                                    tracing::warn!("clipboard stage write failed: {e}");
                                }
                            }
                        }
                        // A content kind a newer build introduced — drop the
                        // bytes (we've nowhere to put them).
                        ClipboardContentKind::Unknown => {}
                    }
                }
                // `t`'s borrow ends above; only now can the map be mutated.
                if over {
                    inbound.remove(&key);
                    tracing::warn!("clipboard transfer exceeded cap — dropped");
                }
            }
            ClipboardEvent::Close { transfer } => {
                let entry = self.clip_inbound.lock().remove(&(route, transfer));
                let Some(t) = entry else {
                    return;
                };
                match t.content {
                    ClipboardContentKind::Image => self.clipboard.set_image(t.image),
                    ClipboardContentKind::Files => {
                        let dir = crate::clipboard::staging_dir(transfer);
                        let paths = t
                            .items
                            .iter()
                            .map(|i| dir.join(safe_name(&i.name)).to_string_lossy().into_owned())
                            .collect();
                        self.clipboard.set_files(paths);
                    }
                    // A content kind a newer build introduced — nothing to
                    // commit to the OS clipboard.
                    ClipboardContentKind::Unknown => {}
                }
            }
            // Pull is handled by the caller (sink side only); a newer build's
            // event is ignored rather than failing the frame.
            ClipboardEvent::Pull | ClipboardEvent::Unknown => {}
        }
    }

    /// Fan one room-plane message out to the given members — the rooms
    /// channel's point-to-point sends (an invite, a join/leave, a chat
    /// line). Best-effort per member: one with no shared network right now
    /// (offline, or never seen) is skipped — the rooms plane has no acks,
    /// and presence plus re-stated invites heal the gaps. Returns how many
    /// members the daemon actually dispatched to, so the UI can be honest
    /// about a line that reached nobody.
    pub async fn room_send(
        &self,
        members: Vec<String>,
        message: RoomMessage,
    ) -> Result<u32, String> {
        let me = self.local_node_id();
        let payload = serde_json::to_value(&message).map_err(|e| e.to_string())?;
        let mut delivered = 0u32;
        for member in members {
            // Never loop a message back at ourselves (the GUI already
            // applied it locally).
            if me
                .as_deref()
                .is_some_and(|m| pubkey_part(m) == pubkey_part(&member))
            {
                continue;
            }
            let Some(network) = self.network_for_peer(&member) else {
                continue;
            };
            let resp = self
                .client
                .request(&Request::ChannelSendTo {
                    network,
                    channel: CHANNEL_ROOMS.to_string(),
                    peer: pubkey_part(&member).to_string(),
                    payload: payload.clone(),
                })
                .await;
            match resp {
                Ok(r) if r.ok => delivered += 1,
                Ok(r) => tracing::debug!(
                    "room send to {} refused: {}",
                    short_id(&member),
                    r.error.unwrap_or_default()
                ),
                Err(e) => tracing::debug!("room send to {} failed: {e}", short_id(&member)),
            }
        }
        Ok(delivered)
    }

    /// Send a control message to one peer, reporting whether the daemon
    /// actually dispatched it. The daemon's peer set is keyed by the *bare
    /// pubkey* (what signaling announces), while AllMyStuff mostly holds
    /// display ids (`pubkey-SUFFIX`, what presence and `IdentityShow` carry)
    /// — so the id is canonicalised here, at the daemon boundary. Addressing
    /// the display form made every send come back "peer not found", an error
    /// this used to swallow: a claim showed "asking…" and then nothing.
    async fn send_control(&self, peer: &str, message: &ControlMessage) -> Result<(), String> {
        let Some(network) = self.network_for_peer(peer) else {
            return Err(format!("no shared network with {peer}"));
        };
        let payload = serde_json::to_value(message).map_err(|e| e.to_string())?;
        let resp = self
            .client
            .request(&Request::ChannelSendTo {
                network,
                channel: CHANNEL_CONTROL.to_string(),
                peer: pubkey_part(peer).to_string(),
                payload,
            })
            .await
            .map_err(|e| e.to_string())?;
        if resp.ok {
            Ok(())
        } else {
            let err = resp.error.unwrap_or_else(|| "channel send failed".into());
            tracing::warn!("control send to {peer} failed: {err}");
            Err(err)
        }
    }

    fn emit_snapshot(&self) {
        self.sink.emit("allmystuff://session", self.snapshot());
    }

    fn emit_status(&self, status: &str, error: Option<&str>) {
        self.sink.emit(
            "allmystuff://subscription",
            json!({ "status": status, "error": error }),
        );
    }
}

/// A well-formed but empty owned roster (no fleet yet).
fn empty_owned() -> Value {
    json!({ "key": "", "version": 0, "members": [], "is_owner": false, "network_id": "" })
}

/// AllMyStuff's fleet role-grant policy — deliberately **narrower** than the
/// MyOwnMesh closed-network substrate it rides on. MyOwnMesh's `Role::can_grant`
/// lets a controller grant a controller (rank ≥ rank); AllMyStuff does not.
///
/// The clarified roles: a **member** is a node that can *be* controlled, a
/// **manager** (a MyOwnMesh "controller") is a node that *can* control, and an
/// **owner** controls *and* sets managers and owners. So a manager may grant the
/// **member** role only; setting managers or owners is the owner's alone (and an
/// owner grant additionally needs a human at an owner device — the custody
/// second factor, enforced by the daemon). `granter` / `target` are the
/// AllMyStuff UI terms ("member" | "manager" | "owner").
fn ams_may_grant(granter: &str, target: &str) -> bool {
    matches!((granter, target), ("owner", _) | ("manager", "member"))
}

/// The fleet network's display label. A fleet is a closed network owned by the
/// originating node, so when it carries an owner name it reads "<name>'s
/// Fleet"; unnamed, the label is empty and MyOwnMesh falls back to the
/// word-salad network id (the human-communicable name derived from the key).
fn fleet_label(name: &str) -> String {
    let name = name.trim();
    if name.is_empty() {
        String::new()
    } else {
        format!("{name}'s Fleet")
    }
}

// The shape video takes on a console window's IPC channel: a fixed
// 28-byte little-endian header, then the payload. No JSON, no base64;
// the webview hands the bytes straight to a decoder (or, for kind 3,
// straight to the canvas). The route isn't carried — the channel itself
// is per-route.
//
//   [0]      kind: 1 = JPEG frame, 2 = H.264 access unit, 3 = raw RGBA
//   [1]      flags: bit 0 = key (H.264 IDR)
//   [2..4]   reserved
//   [4..8]   width  (JPEG/raw — an H.264 unit carries its size in the SPS)
//   [8..12]  height
//   [12..16] source_width  (JPEG only)
//   [16..20] source_height
//   [20..28] JPEG: frame seq · H.264/raw: timestamp in µs

pub(crate) const VIDEO_IPC_HEADER_LEN: usize = 28;

/// One comparable string for "what this machine advertises": the presence
/// summary + capability list, serialized. The inventory watcher diffs it
/// across rescans — JSON equality is exactly "would peers see something
/// different", since this *is* what presence carries.
fn profile_fingerprint(
    summary: &impl serde::Serialize,
    capabilities: &impl serde::Serialize,
) -> String {
    serde_json::to_string(&(summary, capabilities)).unwrap_or_default()
}

pub(crate) fn video_ipc_header(
    kind: u8,
    flags: u8,
    dims: [u32; 4],
    tail: u64,
    payload_len: usize,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(VIDEO_IPC_HEADER_LEN + payload_len);
    out.push(kind);
    out.push(flags);
    out.extend_from_slice(&[0u8; 2]);
    for d in dims {
        out.extend_from_slice(&d.to_le_bytes());
    }
    out.extend_from_slice(&tail.to_le_bytes());
    out
}

fn video_ipc_bytes(f: &VideoFrame) -> Vec<u8> {
    let mut out = video_ipc_header(
        1,
        0,
        [f.width, f.height, f.source_width, f.source_height],
        f.seq,
        f.jpeg.len(),
    );
    out.extend_from_slice(&f.jpeg);
    out
}

fn h264_ipc_bytes(ts_us: u64, key: bool, data: &[u8]) -> Vec<u8> {
    let mut out = video_ipc_header(2, key as u8, [0; 4], ts_us, data.len());
    out.extend_from_slice(data);
    out
}

/// Node id from a capability id (`"<node>:<device>"`). The node segment is
/// everything before the first colon.
fn node_of(cap_id: &str) -> String {
    cap_id
        .split_once(':')
        .map(|(n, _)| n.to_string())
        .unwrap_or_else(|| cap_id.to_string())
}

/// The device part of a capability id — everything after the node
/// (`"<node>:cam:video0"` → `"cam:video0"`). `None` for a bare node id.
fn device_of(cap_id: &str) -> Option<String> {
    cap_id.split_once(':').map(|(_, dev)| dev.to_string())
}

/// The transport's name for route-active log lines.
fn mode_label(mode: VideoMode) -> &'static str {
    match mode {
        VideoMode::H264 => "H.264 track",
        VideoMode::Mjpeg => "MJPEG",
    }
}

/// Whether `route` is a mesh-native terminal session: generic media whose
/// source endpoint is a machine's `…:terminal` handle. (Terminal
/// endpoints are deliberately *not* catalog capabilities — generic would
/// match every auto-wiring picker — so the shape of the route is the
/// contract.)
fn is_terminal_route(route: &Route) -> bool {
    route.media == MediaKind::Generic && route.from.as_str().ends_with(":terminal")
}

/// Whether `route` is a mesh-native file session: generic media whose
/// source endpoint is a machine's `…:files` handle — the same shape-as-
/// contract scheme the terminal uses.
fn is_files_route(route: &Route) -> bool {
    route.media == MediaKind::Generic && route.from.as_str().ends_with(":files")
}

/// Whether `route` is a room **Shared Files** fetch session: generic media
/// whose source endpoint is a machine's `…:shared` handle. Unlike a files
/// route it is *not* owner/fleet gated — any room member may open one — but
/// it can only `Fetch` by token (see [`FilesPlane`] callers); the host
/// gates each fetch on the token's allow-list, so it never browses a disk.
fn is_shared_route(route: &Route) -> bool {
    route.media == MediaKind::Generic && route.from.as_str().ends_with(":shared")
}

/// Whether `route` is a site (reverse-proxy) session: generic media whose
/// source endpoint is a machine's `…:site` handle — the same shape-as-
/// contract scheme the terminal and files use.
fn is_site_route(route: &Route) -> bool {
    route.media == MediaKind::Generic && route.from.as_str().ends_with(":site")
}

/// What an audio route this machine sources should capture: the synthetic
/// `system-audio` capability advertises "what this machine plays", so it
/// captures the machine's own output (loopback); every other audio source
/// is a scanned input device — the default mic in v1. Pure, so the rule
/// that decides between "your room" and "your sound" is unit-testable.
fn audio_capture_source(route: &Route) -> CaptureSource {
    match route.from.as_str().split_once(':') {
        Some((_, "system-audio")) => CaptureSource::System,
        _ => CaptureSource::Mic,
    }
}

/// Why an inbound terminal/files offer must be refused, if it must: it
/// asks *this* machine to host a shell (or hand over its disk) and the
/// offerer isn't an authorized controller. `None` = fine (not a
/// privileged offer, not our side to host, or the sender is owner/fleet).
/// Pure, so the rule that guards the most privileged things on the mesh
/// is unit-testable.
fn privileged_offer_refusal(
    route: &Route,
    hosts_here: bool,
    sender_may_control: bool,
) -> Option<String> {
    if !hosts_here || sender_may_control {
        return None;
    }
    if is_terminal_route(route) {
        return Some("not authorized: terminal access is owner/fleet only".into());
    }
    if is_files_route(route) {
        return Some("not authorized: file access is owner/fleet only".into());
    }
    if is_site_route(route) {
        return Some("not authorized: site access is owner/fleet only".into());
    }
    None
}

/// `dir/name`, made unique the Finder way: `name (2).ext`, `name (3).ext`…
/// when something already sits there.
fn unique_path(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let p = std::path::Path::new(name);
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| name.to_string());
    let ext = p.extension().map(|e| e.to_string_lossy().into_owned());
    for n in 2.. {
        let candidate = match &ext {
            Some(ext) => dir.join(format!("{stem} ({n}).{ext}")),
            None => dir.join(format!("{stem} ({n})")),
        };
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!()
}

/// The stable pubkey portion of a mesh device id — strip MyOwnMesh's trailing
/// 5-char display suffix (`-AB12C`). Mirrors the core's `signing::pubkey_part`,
/// so a device id in display form (`pubkey-SUFFIX`, what `IdentityShow` and
/// presence use) and bare form (`pubkey`, what the daemon delivers as a
/// channel `from`) compare equal.
fn pubkey_part(id: &str) -> &str {
    if let Some((body, suffix)) = id.rsplit_once('-') {
        if suffix.len() == 5 && suffix.chars().all(|c| c.is_ascii_alphanumeric()) {
            return body;
        }
    }
    id
}

/// A fresh opaque fetch token for one shared file — 16 random bytes as
/// hex, so it can't be guessed and never leaks the path it stands for.
fn fresh_share_token() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::getrandom(&mut bytes).is_err() {
        // RNG unavailable (vanishingly rare): a wall-clock nonce still
        // makes a unique-enough token for one app run.
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(1);
        return format!("share_{n:032x}");
    }
    let mut s = String::with_capacity(6 + 32);
    s.push_str("share_");
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// A fresh random boot id for this app run — never 0, which presence
/// reserves for older peers without the field.
fn fresh_boot_id() -> u64 {
    let mut bytes = [0u8; 8];
    if getrandom::getrandom(&mut bytes).is_err() {
        // RNG unavailable (vanishingly rare): fall back to wall-clock nanos
        // — uniqueness across restarts is all this id needs.
        return std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(1)
            .max(1);
    }
    u64::from_le_bytes(bytes).max(1)
}

/// Log-friendly head of a mesh id — enough to tell two machines apart in a
/// trace without drowning it in base32.
fn short_id(id: &str) -> String {
    if id.len() > 10 {
        format!("{}…", &id[..10])
    } else {
        id.to_string()
    }
}

/// Log-friendly tail of a fleet key — enough to compare two machines' logs
/// ("do we hold the same key?") without printing the grouping secret.
fn key_tail(key: &str) -> &str {
    let n = key.len();
    if n > 6 {
        &key[n - 6..]
    } else {
        key
    }
}

/// Most bytes a single clipboard paste may move across — a guard against a
/// pathological "copy a huge folder, paste over the mesh". Generous for real
/// copy/paste (documents, images, a handful of files).
const MAX_CLIPBOARD_BYTES: u64 = 256 * 1024 * 1024;

/// How long the controlled side waits after a copy/cut keystroke before
/// reading its clipboard for a [`Pull`](ClipboardEvent::Pull) reply — the
/// beat an app needs to actually land the copied selection on the OS
/// clipboard. The keystroke arrives just ahead of the pull on the same
/// ordered channel; this covers the asynchronous gap after injection.
const CLIPBOARD_COPY_SETTLE: std::time::Duration = std::time::Duration::from_millis(120);

/// How long after sending a [`Pull`](ClipboardEvent::Pull) the controller
/// will accept the reply onto its own clipboard. Generous for the round trip
/// plus the settle above; outside it, a clipboard frame on a route we source
/// is unsolicited and dropped.
const CLIPBOARD_PULL_WINDOW: std::time::Duration = std::time::Duration::from_secs(10);

/// An inbound clipboard transfer being reassembled (see
/// [`Mesh::handle_clipboard_frame`]).
struct ClipInbound {
    content: ClipboardContentKind,
    items: Vec<ClipboardItem>,
    /// Per-item: whether its staging file exists yet — so the first chunk
    /// truncates and the rest append.
    started: Vec<bool>,
    /// Accumulated bytes for an image transfer (files stream to disk).
    image: Vec<u8>,
    /// Running total, enforced against [`MAX_CLIPBOARD_BYTES`].
    received: u64,
}

impl ClipInbound {
    fn new(content: ClipboardContentKind, items: Vec<ClipboardItem>) -> Self {
        let n = items.len();
        ClipInbound {
            content,
            items,
            started: vec![false; n],
            image: Vec::new(),
            received: 0,
        }
    }
}

/// Keep only a path's final component, so a crafted item name can't write
/// outside the staging dir.
fn safe_name(name: &str) -> String {
    Path::new(name)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into())
}

/// Append one staging-file piece — the first chunk creates+truncates, the
/// rest append.
fn append_chunk(path: &Path, data: &[u8], first: bool) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).write(true);
    if first {
        opts.truncate(true);
    } else {
        opts.append(true);
    }
    opts.open(path)?.write_all(data)
}

fn parse_media(s: &str) -> MediaKind {
    match s {
        "audio" => MediaKind::Audio,
        "video" => MediaKind::Video,
        "display" => MediaKind::Display,
        "input" => MediaKind::Input,
        "storage" => MediaKind::Storage,
        "clipboard" => MediaKind::Clipboard,
        _ => MediaKind::Generic,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term_route(from: &str, to: &str, media: MediaKind) -> Route {
        Route {
            id: format!("route:{from}→{to}"),
            from: from.into(),
            to: to.into(),
            media,
        }
    }

    struct NoopSink;
    impl UiSink for NoopSink {
        fn emit(&self, _event: &str, _payload: Value) {}
        fn restart(&self) -> ! {
            unreachable!("test sink never restarts")
        }
    }

    /// AllMyStuff's fleet role-grant policy is narrower than MyOwnMesh's: an
    /// owner sets anything, a manager may grant the **member** role only, and a
    /// plain member grants nothing. This is the "controllers grant members only,
    /// even though the underlying myownmesh allows more" rule.
    #[test]
    fn ams_grant_policy_limits_managers_to_members() {
        // Owners set every role.
        assert!(ams_may_grant("owner", "owner"));
        assert!(ams_may_grant("owner", "manager"));
        assert!(ams_may_grant("owner", "member"));
        // Managers may grant members — and only members.
        assert!(ams_may_grant("manager", "member"));
        assert!(!ams_may_grant("manager", "manager"));
        assert!(!ams_may_grant("manager", "owner"));
        // Members grant nothing.
        assert!(!ams_may_grant("member", "member"));
        assert!(!ams_may_grant("member", "manager"));
        assert!(!ams_may_grant("member", "owner"));
    }

    /// Regression guard for the GUI crash where `Mesh::new` spawned its media
    /// forwarders inline: the desktop app builds the `Mesh` in a *synchronous*
    /// Tauri `setup` with no ambient Tokio runtime, so a `tokio::spawn` in
    /// `new` panics with "there is no reactor running". This is a plain
    /// `#[test]` (no `#[tokio::test]`) precisely so it runs without a runtime —
    /// if `new` ever spawns again it will panic here. The forwarders are
    /// deferred to `start`, which is always called from an async context.
    #[test]
    fn new_does_not_require_a_running_tokio_runtime() {
        let client = Arc::new(ControlClient::new().expect("resolve control socket path"));
        let _mesh = Mesh::new(client, Arc::new(NoopSink));
    }

    /// Regression guard for the screen/audio outage: the engine fires tasks
    /// from capture/audio OS threads (e.g. the DXGI status callback), where a
    /// bare `tokio::spawn` panics with "no reactor running". Every engine spawn
    /// goes through [`crate::spawn`], which must work off-runtime via the handle
    /// `start` registers. Spawn from a plain `std::thread` (no ambient runtime)
    /// and confirm the task actually runs.
    #[test]
    fn engine_spawn_runs_tasks_from_a_non_runtime_thread() {
        let rt = tokio::runtime::Runtime::new().expect("build runtime");
        crate::set_runtime(rt.handle().clone());
        // Keep the runtime (and the registered handle) alive for the process —
        // OnceLock holds the handle, and this is the only test that sets it.
        std::mem::forget(rt);

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            // No ambient runtime here — `tokio::spawn` would panic.
            crate::spawn(async move {
                let _ = tx.send(());
            });
        })
        .join()
        .unwrap();

        rx.recv_timeout(std::time::Duration::from_secs(5))
            .expect("spawned task should run on the registered runtime");
    }

    /// Two routes attaching to one terminal session — the multi-attach
    /// contract the mesh now drives — both see the shell's output, either can
    /// type into the one shell, and the host's session list reports them as a
    /// single shared session. This drives the same [`TerminalHost::open`] the
    /// `start_terminal_host` pump uses (without needing a live daemon), so it
    /// guards the mesh's view of sharing end to end.
    #[cfg(unix)]
    #[test]
    fn two_routes_share_one_session_through_the_host() {
        use crate::terminal::OutMsg;
        use std::time::{Duration, Instant};

        // The mesh's idle reaper / spawns need a runtime registered.
        let rt = tokio::runtime::Runtime::new().expect("build runtime");
        crate::set_runtime(rt.handle().clone());
        std::mem::forget(rt);

        let client = Arc::new(ControlClient::new().expect("resolve control socket path"));
        let mesh = Mesh::new(client, Arc::new(NoopSink));

        // First route creates the session; second attaches to the same id —
        // exactly what an Offer carrying `session: Some(id)` resolves to.
        let a = mesh
            .terminal
            .open(Some("shared"), "routeA", 80, 24)
            .expect("create session");
        assert!(a.created, "first open creates the session");
        let b = mesh
            .terminal
            .open(Some("shared"), "routeB", 80, 24)
            .expect("attach to session");
        assert!(!b.created, "second open attaches to the shared session");

        // The host's picker list reports one shared session with two viewers.
        let infos = mesh.terminal_session_infos();
        let shared = infos
            .iter()
            .find(|s| s.session_id == "shared")
            .expect("session listed");
        assert_eq!(shared.attachers, 2, "both routes counted as attachers");

        // Either route can type into the one shell, and both pumps see it.
        let mut rxa = a.rx;
        let mut rxb = b.rx;
        assert!(mesh.terminal.write("routeB", b"echo via-B\n".to_vec()));

        let saw = |rx: &mut tokio::sync::broadcast::Receiver<OutMsg>, needle: &str| -> bool {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut seen = Vec::new();
            while Instant::now() < deadline {
                match rx.try_recv() {
                    Ok(OutMsg::Data(b)) => {
                        seen.extend_from_slice(&b);
                        if String::from_utf8_lossy(&seen).contains(needle) {
                            return true;
                        }
                    }
                    Ok(OutMsg::Resize { .. }) => {}
                    Ok(OutMsg::Exit(_)) => return false,
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                        std::thread::sleep(Duration::from_millis(20))
                    }
                    Err(_) => return false,
                }
            }
            false
        };
        assert!(saw(&mut rxa, "via-B"), "route A sees route B's input");
        assert!(saw(&mut rxb, "via-B"), "route B sees its own echo");

        // Detaching one viewer keeps the shell alive for the other.
        mesh.terminal.detach("routeA");
        assert_eq!(
            mesh.terminal_session_infos()
                .iter()
                .find(|s| s.session_id == "shared")
                .map(|s| s.attachers),
            Some(1),
            "session survives one detach with the remaining attacher",
        );
        mesh.terminal.close("shared");
    }

    #[test]
    fn dedup_collapses_duplicate_terminal_frames_by_seq() {
        // The dedup that collapses a frame delivered on several shared
        // networks back to one (both directions): the sending side numbers a
        // route's frames strictly increasing, so a seq already taken is a
        // duplicate. A different route, and the other direction's map, each
        // keep an independent counter.
        let client = Arc::new(ControlClient::new().expect("resolve control socket path"));
        let mesh = Mesh::new(client, Arc::new(NoopSink));
        let out = &mesh.term_rx_seq;
        let inp = &mesh.term_in_seq;

        assert!(Mesh::accept_term_seq(out, "r", 0), "first frame is fresh");
        assert!(
            !Mesh::accept_term_seq(out, "r", 0),
            "same seq again is a duplicate"
        );
        assert!(Mesh::accept_term_seq(out, "r", 1), "the next seq is fresh");
        assert!(
            !Mesh::accept_term_seq(out, "r", 1),
            "and its duplicate drops"
        );
        assert!(
            !Mesh::accept_term_seq(out, "r", 0),
            "an older straggler drops too"
        );
        assert!(Mesh::accept_term_seq(out, "r", 2), "advancing is fresh");
        assert!(
            Mesh::accept_term_seq(out, "r", 9),
            "a forward jump (sender skipped after a lag) is still fresh"
        );
        assert!(
            Mesh::accept_term_seq(out, "other", 0),
            "a different route has its own independent counter"
        );
        // The input map (host taking keystrokes) is wholly independent of the
        // output map — the same route+seq is fresh again here.
        assert!(
            Mesh::accept_term_seq(inp, "r", 0),
            "input dedup is independent of output dedup"
        );
        assert!(
            !Mesh::accept_term_seq(inp, "r", 0),
            "but still drops its own duplicates"
        );
    }

    #[test]
    fn terminal_routes_are_recognized_by_shape() {
        // Generic media + a `…:terminal` source = a terminal session.
        let term = term_route("host:terminal", "me:term-view:1", MediaKind::Generic);
        assert!(is_terminal_route(&term));

        // Generic data that isn't a terminal stays untouched (the escape
        // hatch keeps working for whatever apps wire through it)…
        let generic = term_route("host:thing", "me:other", MediaKind::Generic);
        assert!(!is_terminal_route(&generic));

        // …and a `:terminal` id under any *other* media is not a terminal
        // (the media is part of the contract, not just the suffix).
        let display = term_route("host:terminal", "me:term-view:1", MediaKind::Display);
        assert!(!is_terminal_route(&display));
    }

    #[test]
    fn loopback_terminal_route_is_recognized_as_self_hosted() {
        // The id the front-end mints for "open a terminal to the machine I'm
        // sitting at": both endpoints are this node, source is `…:terminal`.
        let me = "me";
        let route = term_route(
            &format!("{me}:terminal"),
            &format!("{me}:term-view:1"),
            MediaKind::Generic,
        );
        // It's a terminal route…
        assert!(is_terminal_route(&route));
        // …and the loopback predicate the new branch keys on (both ends are
        // this node) holds — so `start_media` takes the loopback path and
        // `term_send` short-circuits input/resize to the local PTY rather
        // than framing it to a peer.
        let from_node = node_of(route.from.as_str());
        let to_node = node_of(route.to.as_str());
        assert_eq!(from_node, me);
        assert_eq!(to_node, me);
        assert!(
            from_node == me && to_node == me,
            "a self-terminal is a loopback route"
        );

        // A remote terminal (viewer here, shell elsewhere) is NOT loopback —
        // it keeps the framed-to-peer path.
        let remote = term_route(
            "host:terminal",
            &format!("{me}:term-view:2"),
            MediaKind::Generic,
        );
        assert!(is_terminal_route(&remote));
        assert_ne!(node_of(remote.from.as_str()), node_of(remote.to.as_str()));
    }

    #[test]
    fn files_routes_are_recognized_by_shape() {
        let files = term_route("host:files", "me:files-view:1", MediaKind::Generic);
        assert!(is_files_route(&files));
        assert!(!is_terminal_route(&files), "files ≠ terminal");

        let generic = term_route("host:thing", "me:other", MediaKind::Generic);
        assert!(!is_files_route(&generic));

        let storage = term_route("host:files", "me:files-view:1", MediaKind::Storage);
        assert!(!is_files_route(&storage), "media is part of the contract");
    }

    #[test]
    fn shared_routes_are_recognized_and_distinct_from_files() {
        let shared = term_route("host:shared", "me:shared-view:1", MediaKind::Generic);
        assert!(is_shared_route(&shared));
        // A shared route is *not* a files route — that's the whole point:
        // it skips the owner/fleet offer screen and is fetch-by-token only.
        assert!(!is_files_route(&shared));
        assert!(!is_terminal_route(&shared));

        let files = term_route("host:files", "me:files-view:1", MediaKind::Generic);
        assert!(!is_shared_route(&files));

        // The media is part of the contract here too.
        let storage = term_route("host:shared", "me:shared-view:1", MediaKind::Storage);
        assert!(!is_shared_route(&storage));
    }

    #[test]
    fn capability_ids_split_into_node_and_device() {
        // The device part keeps its own colons — a camera route resolves
        // `<node>:cam:video0` back to the inventory id `cam:video0`, the
        // display arm reads `screen:<id>` the same way.
        assert_eq!(node_of("desk:cam:video0"), "desk");
        assert_eq!(device_of("desk:cam:video0").as_deref(), Some("cam:video0"));
        assert_eq!(device_of("desk:screen:7").as_deref(), Some("screen:7"));
        assert_eq!(device_of("desk:screen").as_deref(), Some("screen"));
        // A bare node id has no device half.
        assert_eq!(device_of("desk"), None);
        assert_eq!(node_of("desk"), "desk");
    }

    #[test]
    fn privileged_offers_are_refused_exactly_when_unauthorized() {
        let term = term_route("me:terminal", "them:term-view:1", MediaKind::Generic);
        let files = term_route("me:files", "them:files-view:1", MediaKind::Generic);

        // Our shell/disk + an unauthorized sender = refusal with a human
        // reason naming the right plane.
        let refusal = privileged_offer_refusal(&term, true, false);
        assert!(refusal.is_some_and(|r| r.contains("terminal") && r.contains("owner/fleet")));
        let refusal = privileged_offer_refusal(&files, true, false);
        assert!(refusal.is_some_and(|r| r.contains("file") && r.contains("owner/fleet")));

        // Owner/fleet senders pass.
        assert_eq!(privileged_offer_refusal(&term, true, true), None);
        assert_eq!(privileged_offer_refusal(&files, true, true), None);

        // An offer that doesn't ask us to host (we'd be the viewer) is no
        // grounds for refusal…
        assert_eq!(privileged_offer_refusal(&term, false, false), None);
        assert_eq!(privileged_offer_refusal(&files, false, false), None);

        // …and unprivileged offers are never screened here, whoever asks.
        let audio = term_route("me:mic", "them:speaker", MediaKind::Audio);
        assert_eq!(privileged_offer_refusal(&audio, true, false), None);

        // A Shared Files (`:shared`) offer is deliberately *not* screened —
        // any room member opens one, and the per-fetch token gate (not the
        // owner/fleet rule) is what keeps it to explicitly-shared files.
        let shared = term_route("me:shared", "them:shared-view:1", MediaKind::Generic);
        assert_eq!(privileged_offer_refusal(&shared, true, false), None);
    }

    #[test]
    fn fresh_share_tokens_are_unguessable_and_unique() {
        let a = fresh_share_token();
        let b = fresh_share_token();
        assert!(a.starts_with("share_"));
        assert_ne!(a, b, "tokens must not collide");
        // 16 random bytes as hex, after the `share_` prefix.
        assert_eq!(a.len(), "share_".len() + 32);
        assert!(a["share_".len()..].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn unique_path_counts_the_finder_way() {
        let dir = std::env::temp_dir().join(format!(
            "amst-unique-test-{}-{}",
            std::process::id(),
            fresh_boot_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(unique_path(&dir, "a.txt"), dir.join("a.txt"));
        std::fs::write(dir.join("a.txt"), b"x").unwrap();
        assert_eq!(unique_path(&dir, "a.txt"), dir.join("a (2).txt"));
        std::fs::write(dir.join("a (2).txt"), b"x").unwrap();
        assert_eq!(unique_path(&dir, "a.txt"), dir.join("a (3).txt"));
        std::fs::write(dir.join("noext"), b"x").unwrap();
        assert_eq!(unique_path(&dir, "noext"), dir.join("noext (2)"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn system_audio_routes_capture_the_machines_own_output() {
        // The synthetic `system-audio` capability = "what this machine
        // plays" — its routes loop the output back…
        let system = term_route("me:system-audio", "them:system-audio", MediaKind::Audio);
        assert_eq!(audio_capture_source(&system), CaptureSource::System);

        // …while a scanned input device (and anything unrecognized,
        // including a bare node id) captures the mic, exactly as before.
        let mic = term_route("me:mic:array-1", "them:system-audio", MediaKind::Audio);
        assert_eq!(audio_capture_source(&mic), CaptureSource::Mic);
        let bare = term_route("me", "them:system-audio", MediaKind::Audio);
        assert_eq!(audio_capture_source(&bare), CaptureSource::Mic);
    }
}
