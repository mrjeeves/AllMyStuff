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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;

use allmystuff_graph::{MediaKind, NodeId, Route};
use allmystuff_protocol::{
    ClientId, ControlMessage, NodeProfile, OwnedRoster, OwnershipControl, Request, RoomMessage,
    RouteControl, CHANNEL_CONTROL, CHANNEL_MEDIA, CHANNEL_OWNED, CHANNEL_PRESENCE, CHANNEL_ROOMS,
    PROTOCOL_VERSION,
};
use allmystuff_session::{
    AudioFrame, Effect, FileEvent, FileFrame, InputAction, InputEvent, MediaPayload, Session,
    TermEvent, TermFrame, VideoAssembler, VideoFrame, VideoStatusFrame,
};

use crate::audio::{AudioBridge, CaptureSource};
use crate::control_client::{ControlClient, MediaPipe};
use crate::files::FilesPlane;
use crate::input_inject::Injector;
use crate::ownership::Ownership;
use crate::terminal::{OutMsg, TerminalHost};
use crate::video::{VideoBridge, VideoMode, VideoPacket};
use crate::video_decode::{Au, DecodeBridge};

pub struct Mesh {
    client: Arc<ControlClient>,
    /// The media plane's dedicated daemon connection: frame chunks ride it
    /// back-to-back instead of paying a connect + round trip each.
    media_pipe: MediaPipe,
    app: AppHandle,
    audio: Arc<AudioBridge>,
    /// Screen capture for display routes this machine sources (the far end
    /// of a console session looking at us).
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
    /// Mesh-native file sessions: filesystem ops this machine hosts for
    /// files routes sourcing here (gated like the terminal), and the
    /// response buffers files windows drain for routes sinking here.
    files: FilesPlane,
    /// Sequence for outbound file frames (requests viewer-side, response
    /// streams host-side — one stream per app run, like `term_seq`).
    file_seq: AtomicU64,
    /// Viewer-side download sinks: a `(route, req)` whose `Chunk`s should
    /// stream straight to a local file (the Downloads folder) instead of
    /// the window's queue — registered by `file_download` *before* the
    /// Read request goes out, so the first chunk can't race it.
    downloads: Mutex<HashMap<(String, u64), DownloadSink>>,
    state: Mutex<State>,
    /// This device's persisted ownership record — who owns it and whether
    /// it's currently offering itself for adoption (claim mode).
    ownership: Arc<Ownership>,
    /// Outbound audio: capture callbacks push `(peer, frame)`; a forwarder
    /// task sends them on the media channel. Bounded like video: a stalled
    /// link sheds buffers (a brief skip) instead of queueing a backlog the
    /// listener then hears seconds late.
    audio_out: mpsc::Sender<AudioOut>,
    /// Outbound video, deliberately *bounded*: when the link can't keep up
    /// the capture side drops frames instead of queueing stale ones (an
    /// MJPEG drop costs freshness only; an H.264 drop is healed by the
    /// next forced IDR).
    video_out: mpsc::Sender<(String, VideoPacket)>,
    /// Sequence for outbound input events (one stream per app run).
    input_seq: AtomicU64,
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
    /// The H.264 track lane is one per peer connection: which route is
    /// streaming *out* on it (peer pubkey → route id). A second display
    /// route to the same peer falls back to MJPEG until the lane frees.
    video_lane_out: Mutex<HashMap<String, String>>,
    /// Which inbound route consumes each peer's track lane here (peer
    /// pubkey → route id) — set when our offered display route goes
    /// active with `h264` accepted, so `video_inbound` events route to
    /// the right console window.
    video_lane_in: Mutex<HashMap<String, String>>,
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
    /// When each route last asked its sender for a clean decode entry —
    /// decode errors arrive at frame rate; the asks must not.
    refresh_asks: Mutex<HashMap<String, std::time::Instant>>,
    /// The Opus audio lane mirrors the video lane's bookkeeping: one
    /// outbound stream per peer connection (peer pubkey → route id)…
    audio_lane_out: Mutex<HashMap<String, String>>,
    /// …and which inbound route consumes each peer's audio lane here —
    /// claimed when our offered audio route goes active with `opus`
    /// accepted, so `audio_inbound` frames decode into the right ring.
    audio_lane_in: Mutex<HashMap<String, String>>,
    /// Per-route Opus decoders for inbound lane audio (stateful across
    /// frames; dropped with the route).
    audio_decoders: Mutex<HashMap<String, opus::Decoder>>,
    /// Whether the local daemon speaks the audio track lane (`audio_*`
    /// ops, myownmesh ≥ 0.2.4) — the audio twin of `daemon_video`.
    /// While false, audio rides PCM frames over the media channel.
    daemon_audio: std::sync::atomic::AtomicBool,
}

/// One captured-audio packet headed for the forwarder, in whichever
/// shape its route negotiated.
enum AudioOut {
    /// A PCM frame for `CHANNEL_MEDIA` — the floor every peer speaks.
    Channel(String, AudioFrame),
    /// One encoded Opus frame for the daemon's audio track lane.
    Lane { peer: String, data: Vec<u8> },
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
    /// Last presence boot id seen per peer (canonical pubkey). A boot id we
    /// haven't recorded means the peer just (re)started and missed our
    /// adverts — we answer with our state directly. This is what lets
    /// gossip be event-driven instead of a heartbeat.
    peer_boots: HashMap<String, u64>,
    client_id: Option<ClientId>,
    profile: Option<NodeProfile>,
}

impl Mesh {
    pub fn new(client: Arc<ControlClient>, app: AppHandle) -> Arc<Self> {
        // Shallow queues both: at most a few frames in flight, so a slow
        // link sheds load by dropping captures rather than growing latency.
        // Audio's 8 buffers are ~160 ms of slack.
        let (audio_out, mut audio_rx) = mpsc::channel::<AudioOut>(8);
        let (video_out, mut video_rx) = mpsc::channel::<(String, VideoPacket)>(4);
        let mesh = Arc::new(Mesh {
            client: client.clone(),
            media_pipe: MediaPipe::new(client.clone()),
            app,
            audio: Arc::new(AudioBridge::new()),
            video: Arc::new(VideoBridge::new()),
            video_decode: Arc::new(DecodeBridge::new()),
            injector: Injector::new(),
            terminal: TerminalHost::new(),
            term_seq: AtomicU64::new(0),
            files: FilesPlane::new(),
            file_seq: AtomicU64::new(0),
            downloads: Mutex::new(HashMap::new()),
            state: Mutex::new(State {
                session: None,
                network: None,
                networks: Vec::new(),
                peer_networks: HashMap::new(),
                peer_boots: HashMap::new(),
                client_id: None,
                profile: None,
            }),
            ownership: Arc::new(Ownership::load()),
            audio_out,
            video_out,
            input_seq: AtomicU64::new(0),
            boot_id: fresh_boot_id(),
            video_in: Mutex::new(VideoAssembler::new()),
            video_watchers: Mutex::new(HashMap::new()),
            video_lane_out: Mutex::new(HashMap::new()),
            video_lane_in: Mutex::new(HashMap::new()),
            daemon_video: std::sync::atomic::AtomicBool::new(false),
            video_in_stats: Mutex::new(HashMap::new()),
            refresh_asks: Mutex::new(HashMap::new()),
            audio_lane_out: Mutex::new(HashMap::new()),
            audio_lane_in: Mutex::new(HashMap::new()),
            audio_decoders: Mutex::new(HashMap::new()),
            daemon_audio: std::sync::atomic::AtomicBool::new(false),
        });

        // Forwarders: drain captured frames out to peers on the media
        // channel, both bounded (see the field docs). Send
        // failures are *surfaced* (rate-limited): a silently-dying media
        // plane is exactly the "connected but nothing arrives" mystery.
        {
            let mesh = mesh.clone();
            tauri::async_runtime::spawn(async move {
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
                        AudioOut::Lane { peer, data } => {
                            let r = mesh.send_audio_track(&peer, data).await;
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
        {
            let mesh = mesh.clone();
            tauri::async_runtime::spawn(async move {
                let mut last_warn = std::time::Instant::now() - WARN_EVERY;
                while let Some((peer, packet)) = video_rx.recv().await {
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
                            mesh.send_video_track(&peer, data, duration_us).await
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
        mesh
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
                duration_us,
                data: base64::engine::general_purpose::STANDARD.encode(&data),
            })
            .await
            .map_err(|e| e.to_string())
    }

    /// Send one encoded Opus frame to `peer` over the daemon's audio
    /// track lane (base64 on the control socket, RTP on the wire).
    async fn send_audio_track(&self, peer: &str, data: Vec<u8>) -> Result<(), String> {
        use base64::Engine as _;
        let Some(network) = self.network_for_peer(peer) else {
            return Err("no shared network".into());
        };
        self.media_pipe
            .send(&Request::AudioSend {
                network,
                peer: pubkey_part(peer).to_string(),
                duration_us: crate::audio::OPUS_FRAME_US,
                data: base64::engine::general_purpose::STANDARD.encode(&data),
            })
            .await
            .map_err(|e| e.to_string())
    }

    fn network(&self) -> Option<String> {
        self.state.lock().network.clone()
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
        tauri::async_runtime::spawn(async move {
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
        tauri::async_runtime::spawn(async move {
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
            // can't hear them.
            features: vec![
                allmystuff_protocol::FEATURE_TERMINAL.to_string(),
                allmystuff_protocol::FEATURE_FILES.to_string(),
                allmystuff_protocol::FEATURE_ROOMS.to_string(),
            ],
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
                self.handle_video_inbound(from, rtp_timestamp, key, data);
            }
            "audio_inbound" => {
                let from = value.get("from").and_then(|v| v.as_str()).unwrap_or("");
                let Some(data) = value.get("data").and_then(|v| v.as_str()) else {
                    return;
                };
                self.handle_audio_inbound(from, data);
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
                            tauri::async_runtime::spawn(async move {
                                mesh.ownership_check(Some(&device)).await;
                            });
                        }
                    }
                    let _ = self.app.emit("allmystuff://event", event.clone());
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
                    // A boot id we haven't recorded for this peer means its
                    // app just (re)started and missed our adverts — answer
                    // with our presence + roster directly. This (plus the
                    // connection-approved trigger) is what replaced the
                    // periodic re-broadcast; the reply can't loop because
                    // the peer then knows our boot id and stays quiet.
                    // `boot == 0` is an older heartbeating peer: no reply
                    // needed. Our own echo never replies to itself.
                    let canon = pubkey_part(profile.node.as_str()).to_string();
                    let is_self = self
                        .local_node_id()
                        .is_some_and(|me| pubkey_part(&me) == canon);
                    let new_boot = profile.boot != 0 && !is_self && {
                        let mut st = self.state.lock();
                        st.peer_boots.insert(canon, profile.boot) != Some(profile.boot)
                    };
                    let changed = {
                        let mut st = self.state.lock();
                        st.session
                            .as_mut()
                            .map(|s| s.apply_presence(profile))
                            .unwrap_or(false)
                    };
                    if new_boot {
                        tracing::info!(
                            "peer {} (re)started — answering with our presence + roster",
                            short_id(&from)
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
            CHANNEL_MEDIA => {
                let Some(media) = MediaPayload::decode(payload) else {
                    return;
                };
                match media {
                    MediaPayload::Audio(frame) => self.audio.feed(&frame.route, &frame),
                    MediaPayload::Video(frame) => {
                        // Surface frames only for a route this session knows
                        // is live, sinks here, and belongs to the sender —
                        // the watching console window renders them. Chunked
                        // frames reassemble first; the first complete frame
                        // of a stream is logged so "connected but no pixels"
                        // is attributable from this side too.
                        if !self.inbound_media_ok(&frame.route, &from, MediaKind::Display) {
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
                                    "first screen frame for {} ({}×{})",
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
                        // asleep", "waiting for consent"…). Gated like the
                        // frames it stands in for; the console window shows
                        // it on the stage.
                        if !self.inbound_media_ok(&status.route, &from, MediaKind::Display) {
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
                        let _ = self.app.emit(
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
                            self.injector.apply(ev.action);
                        } else {
                            tracing::warn!(
                                "dropped input event from {from}: not an authorized controller"
                            );
                        }
                    }
                    MediaPayload::Terminal(frame) => self.handle_term_frame(&from, frame),
                    MediaPayload::File(frame) => self.handle_file_frame(&from, frame),
                }
            }
            CHANNEL_OWNED => {
                // A peer gossiped its fleet roster. Merge it; if our copy
                // changed (a new member, or we adopted the key as a freshly
                // adopted device), re-broadcast so the fleet converges and
                // tell the front-end.
                if let Ok(roster) = serde_json::from_value::<OwnedRoster>(payload) {
                    let Some(me) = self.local_node_id() else {
                        return;
                    };
                    let structural = self.ownership.merge_fleet(&me, &from, &roster);
                    // Gossip echoes after every presence answer and
                    // re-broadcast; only a roster that *changed* something
                    // is worth a line at the default level.
                    if structural {
                        tracing::info!(
                            "owned roster from {}: key …{} v{} ({} members) → merged",
                            short_id(&from),
                            key_tail(&roster.key),
                            roster.version,
                            roster.members.len(),
                        );
                        self.broadcast_owned().await;
                        self.emit_owned();
                    } else {
                        let outcome = if self.ownership.fleet().is_some_and(|f| f.key == roster.key)
                        {
                            "in sync"
                        } else {
                            "ignored (not our fleet)"
                        };
                        tracing::debug!(
                            "owned roster from {}: key …{} v{} ({} members) → {outcome}",
                            short_id(&from),
                            key_tail(&roster.key),
                            roster.version,
                            roster.members.len(),
                        );
                    }
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
                    let _ = self
                        .app
                        .emit("allmystuff://room", json!({ "from": from, "message": msg }));
                }
            }
            _ => {}
        }
    }

    /// Free any track-lane claims held by a route that just ended, in
    /// both directions — the next display route to that peer can take
    /// the lane over. The route's native decoder (if any) goes with it.
    fn release_video_lanes(&self, route_id: &str) {
        self.video_lane_out.lock().retain(|_, rid| rid != route_id);
        self.video_lane_in.lock().retain(|_, rid| rid != route_id);
        self.video_in_stats.lock().remove(route_id);
        self.refresh_asks.lock().remove(route_id);
        self.video_decode.stop(route_id);
    }

    /// The audio twin of [`Self::release_video_lanes`]: free the route's
    /// audio-lane claims in both directions and drop its Opus decoder.
    fn release_audio_lanes(&self, route_id: &str) {
        self.audio_lane_out.lock().retain(|_, rid| rid != route_id);
        self.audio_lane_in.lock().retain(|_, rid| rid != route_id);
        self.audio_decoders.lock().remove(route_id);
    }

    /// One Opus frame arrived on a peer's audio lane. It belongs to
    /// whichever of our routes claimed that peer's inbound lane — gated
    /// exactly like every other media frame (route live, sinks here,
    /// sender is the route's peer) — then decodes straight into the
    /// route's playback ring.
    fn handle_audio_inbound(self: &Arc<Self>, from: &str, data_b64: &str) {
        use base64::Engine as _;
        let route_id = {
            let lanes = self.audio_lane_in.lock();
            lanes.get(pubkey_part(from)).cloned()
        };
        let Some(route_id) = route_id else {
            tracing::debug!(
                "audio frame from {} with no claimed inbound lane — dropped",
                short_id(from)
            );
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

    /// One assembled H.264 access unit arrived on a peer's track lane.
    /// It belongs to whichever of our routes claimed that peer's inbound
    /// lane — gated exactly like MJPEG frames (route live, sinks here,
    /// sender is the route's peer) before it reaches a console window.
    /// Where it goes next is the watcher's choice: access units straight
    /// through (the webview decodes — WebCodecs), or through the native
    /// decoder, which hands the window ready-to-paint RGBA frames.
    fn handle_video_inbound(
        self: &Arc<Self>,
        from: &str,
        rtp_timestamp: u32,
        key: bool,
        data_b64: &str,
    ) {
        use base64::Engine as _;
        let canon = pubkey_part(from).to_string();
        let Some(route_id) = self.video_lane_in.lock().get(&canon).cloned() else {
            tracing::debug!(
                "video sample from {} with no claimed inbound lane — dropped",
                short_id(from)
            );
            return;
        };
        if !self.inbound_media_ok(&route_id, from, MediaKind::Display) {
            tracing::debug!("video sample for {route_id} refused (route not live here)");
            return;
        }
        let Ok(data) = base64::engine::general_purpose::STANDARD.decode(data_b64) else {
            return;
        };
        self.note_video_in(&route_id, "H.264", data.len());
        // 90 kHz RTP clock → µs for the decoder's timestamps.
        let ts_us = rtp_timestamp as u64 * 1000 / 90;
        let wants_decode = self
            .video_watchers
            .lock()
            .get(&route_id)
            .is_some_and(|w| w.decode);
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
                        tauri::async_runtime::spawn(async move {
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
            tracing::debug!("no console window watching {route_id} — packet dropped");
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
            let _ = self.app.emit("allmystuff://video-ready", route_id);
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
        let _ = self.app.emit("allmystuff://video-ready", route_id);
    }

    /// Front-end command: offer a route from `from` to `to`.
    pub async fn connect(
        self: &Arc<Self>,
        from: String,
        to: String,
        media: String,
        video: Vec<String>,
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
                let _ = s.offer(route.clone(), me.as_str(), Vec::new(), Vec::new());
                s.handle(
                    NodeId::from(me.as_str()),
                    ControlMessage::Route(RouteControl::Accept {
                        route_id: route.id.clone(),
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
            s.offer(route.clone(), peer.as_str(), video, audio)
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
        // offer that goes nowhere has its own warns above and below.
        tracing::debug!(
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
        json!({
            "ready": true,
            "me": me,
            "network": network,
            "peers": peers,
            "routes": routes,
        })
    }

    fn route_peer(&self, route_id: &str) -> Option<String> {
        self.state
            .lock()
            .session
            .as_ref()
            .and_then(|s| s.route(route_id).map(|r| r.peer.to_string()))
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
                Effect::StopMedia(id) => {
                    self.audio.stop(&id);
                    self.video.stop(&id);
                    self.video_watchers.lock().remove(&id);
                    self.release_video_lanes(&id);
                    self.release_audio_lanes(&id);
                    self.terminal.stop(&id);
                    self.files.stop(&id);
                    self.drop_downloads(&id);
                }
                Effect::Share { from, message } => {
                    let _ = self.app.emit(
                        "allmystuff://share",
                        json!({ "from": from.to_string(), "message": message }),
                    );
                }
                Effect::Ownership { from, message } => self.handle_ownership(from, message).await,
            }
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
                // A claim change → run the full status check (the release
                // also cleared our fleet membership, so the empty roster
                // reaches the UI).
                let owner = self.ownership.owner();
                if owner.as_deref().map(pubkey_part) == Some(pubkey_part(from.as_str())) {
                    tracing::info!("released by {} — unowned again", short_id(from.as_str()));
                    self.ownership.set_owner(None);
                    self.ownership_check(None).await;
                }
            }
            OwnershipControl::Claimed { owner } => {
                // The device we claimed (`from`) accepted us as its owner.
                // Make the claim *do* something durable: establish or extend
                // the owned fleet — mint our key on the first adoption, add
                // ourselves and the new device, hand the full roster straight
                // to it, and gossip so every co-owned device converges on the
                // same key + membership. This is the "Owned roster" linking the
                // fleet under a shared key.
                self.ownership.ensure_fleet_key();
                if let Some(me) = self.local_node_id() {
                    let my_label = self.profile_label().unwrap_or_else(|| me.clone());
                    self.ownership.upsert_member(&me, &my_label);
                }
                let label = self.peer_label(&from);
                self.ownership.upsert_member(from.as_str(), &label);
                if let Some(r) = self.ownership.fleet() {
                    tracing::info!(
                        "claim confirmed by {}; fleet key …{} now {} members (v{})",
                        short_id(from.as_str()),
                        key_tail(&r.key),
                        r.members.len(),
                        r.version
                    );
                }
                self.send_owned_to(from.as_str()).await;
                self.broadcast_owned().await;
                self.emit_owned();
                // Surface the claim feedback for the claimer's toast, too.
                let _ = self.app.emit(
                    "allmystuff://ownership",
                    json!({
                        "from": from.to_string(),
                        "message": OwnershipControl::Claimed { owner },
                    }),
                );
            }
            other => {
                // Declined — feedback for the claimer's UI.
                tracing::info!(
                    "ownership reply from {}: {:?}",
                    short_id(from.as_str()),
                    other
                );
                let _ = self.app.emit(
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

    /// Broadcast this device's fleet roster (if any) on the owned channel to
    /// every network, so co-owned devices converge on one key + membership.
    async fn broadcast_owned(&self) {
        let Some(roster) = self.ownership.fleet() else {
            return;
        };
        self.broadcast_roster(&roster).await;
    }

    /// Hand one explicit roster to each of its members individually — never a
    /// blanket `ChannelSendAll`. Used for the final minus-self roster of a
    /// leave (our own store is already cleared) and the bumped roster of a
    /// kick. The roster carries the fleet's grouping key and a receiver only
    /// honours membership from its owner or an existing member, so it goes to
    /// the membership only, never to every peer on the network — that keeps
    /// the key off bystanders and matches what the receiver will accept.
    async fn broadcast_roster(&self, roster: &OwnedRoster) {
        let me = self.local_node_id();
        let me_canon = me.as_deref().map(pubkey_part);
        for member in &roster.members {
            if Some(pubkey_part(member.device.as_str())) == me_canon {
                continue;
            }
            self.send_roster_to(member.device.as_str(), roster).await;
        }
    }

    /// Send this device's fleet roster straight to one peer — used right
    /// after a claim (and on the targeted ownership check) so the device
    /// gets the key + membership the moment it matters.
    async fn send_owned_to(&self, peer: &str) {
        let Some(roster) = self.ownership.fleet() else {
            return;
        };
        self.send_roster_to(peer, &roster).await;
    }

    /// Send one explicit roster straight to a peer — a kick hands the
    /// kicked device the roster it's no longer in, so it drops out
    /// immediately.
    async fn send_roster_to(&self, peer: &str, roster: &OwnedRoster) {
        let Some(network) = self.network_for_peer(peer) else {
            tracing::warn!("no network to hand the fleet roster to {}", short_id(peer));
            return;
        };
        if let Ok(payload) = serde_json::to_value(roster) {
            let resp = self
                .client
                .request(&Request::ChannelSendTo {
                    network: network.clone(),
                    channel: CHANNEL_OWNED.to_string(),
                    peer: pubkey_part(peer).to_string(),
                    payload,
                })
                .await;
            match resp {
                Ok(r) if r.ok => {
                    tracing::info!("fleet roster handed to {} on {network}", short_id(peer));
                }
                Ok(r) => tracing::warn!(
                    "fleet roster to {} refused by daemon: {}",
                    short_id(peer),
                    r.error.unwrap_or_else(|| "(no error)".into())
                ),
                Err(e) => tracing::warn!("fleet roster to {} failed: {e}", short_id(peer)),
            }
        }
    }

    /// Push the current fleet roster to the front-end.
    fn emit_owned(&self) {
        let _ = self
            .app
            .emit("allmystuff://owned", self.owned_roster_value());
    }

    /// The current fleet roster as JSON — for the `owned_roster` command and
    /// the `allmystuff://owned` event. An empty key/members when there's no
    /// fleet yet, so the front-end always gets a well-formed shape.
    pub fn owned_roster_value(&self) -> Value {
        match self.ownership.fleet() {
            Some(r) => serde_json::to_value(r).unwrap_or_else(|_| empty_owned()),
            None => empty_owned(),
        }
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

    /// Front-end command: put *this* device into (or out of) claim mode, so
    /// another of your machines can adopt it. Re-advertises immediately.
    pub async fn set_claimable(self: &Arc<Self>, on: bool) -> Result<bool, String> {
        self.ownership.set_claim_mode(on);
        self.refresh_profile_ownership().await;
        Ok(self.ownership.claimable())
    }

    /// The claim-status check — "is what we believe about ownership still
    /// true, and does everyone else know it?" Drops incoherent fleet
    /// residue, re-stamps the live profile from the ownership store, then
    /// re-asserts presence + roster. Runs **targeted** at one peer right
    /// after its connection establishes or its app (re)starts — so the two
    /// sides converge on the event itself; there is no heartbeat — and
    /// **broadcast** on the local triggers: session start, a claim/release,
    /// and fleet membership changes.
    pub async fn ownership_check(self: &Arc<Self>, peer: Option<&str>) {
        let Some(me) = self.local_node_id() else {
            return;
        };
        if self.ownership.sanitize_fleet(&me) {
            tracing::info!("ownership check dropped a stale fleet roster");
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
                // The roster (it carries the fleet's grouping key) goes only
                // to peers that are actually in the fleet — presence is for
                // everyone, the key is not.
                let member = self.ownership.fleet().is_some_and(|f| {
                    f.members
                        .iter()
                        .any(|m| pubkey_part(m.device.as_str()) == pubkey_part(peer))
                });
                if member {
                    self.send_owned_to(peer).await;
                }
            }
            None => {
                self.broadcast_presence().await;
                self.broadcast_owned().await;
            }
        }
        self.emit_owned();
        self.emit_snapshot();
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

    /// Front-end command: leave the fleet this device belongs to. The
    /// remaining members get the bumped minus-us roster (replacement
    /// semantics drop us everywhere), our own fleet state clears, and —
    /// since membership follows ownership — any recorded owner is let go
    /// and presence re-advertises unowned.
    pub async fn fleet_leave(self: &Arc<Self>) -> Result<(), String> {
        let me = self.local_node_id().ok_or("mesh not ready")?;
        let roster = self
            .ownership
            .leave_fleet(&me)
            .ok_or("this device isn't in a fleet")?;
        tracing::info!(
            "leaving the fleet — broadcasting roster v{} ({} members remain)",
            roster.version,
            roster.members.len()
        );
        self.broadcast_roster(&roster).await;
        if self.ownership.owner().is_some() {
            self.ownership.set_owner(None);
        }
        self.refresh_profile_ownership().await;
        self.emit_owned();
        Ok(())
    }

    /// Front-end command: kick `device` out of the fleet. The store
    /// enforces the rule — only a member may kick, and never itself — and
    /// the kicked device learns immediately: it gets a best-effort
    /// ownership release (honoured when we're its recorded owner) plus the
    /// new roster it's absent from, which its merge treats as "kicked".
    pub async fn fleet_kick(self: &Arc<Self>, device: String) -> Result<(), String> {
        let me = self.local_node_id().ok_or("mesh not ready")?;
        let roster = self.ownership.kick_member(&me, &device)?;
        tracing::info!(
            "kicked {} from the fleet (roster now v{}, {} members)",
            short_id(&device),
            roster.version,
            roster.members.len()
        );
        self.broadcast_roster(&roster).await;
        let _ = self
            .send_control(
                &device,
                &ControlMessage::Ownership(OwnershipControl::Release),
            )
            .await;
        self.send_roster_to(&device, &roster).await;
        self.emit_owned();
        Ok(())
    }

    /// Front-end command: name (or rename) the fleet. The bumped roster
    /// replaces everywhere it gossips — same convergence as a kick — and
    /// the UI refreshes from the `allmystuff://owned` event.
    pub async fn fleet_set_name(self: &Arc<Self>, name: String) -> Result<(), String> {
        let me = self.local_node_id().ok_or("mesh not ready")?;
        let roster = self.ownership.set_fleet_name(&me, &name)?;
        tracing::info!(
            "fleet named {:?} (roster now v{})",
            roster.name,
            roster.version
        );
        self.broadcast_roster(&roster).await;
        self.emit_owned();
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
        self.broadcast_presence().await;
        self.broadcast_owned().await;
        self.emit_snapshot();
    }

    /// Subscribe presence, owned, control, media, and rooms on each given
    /// network. All of them ride every network: broadcasts (presence/owned)
    /// so peers are found wherever they are, and point-to-point
    /// (control/media/rooms) so a frame addressed to whichever network the
    /// *sender* last saw us on always has a subscriber here.
    async fn subscribe_channels(&self, client_id: ClientId, networks: &[String]) {
        let channels = [
            CHANNEL_PRESENCE,
            CHANNEL_OWNED,
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
    /// streaming), and input (remote control) are wired; camera video and
    /// storage still show active without a transport, and the log says so.
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
                    let canon = pubkey_part(&to_node).to_string();
                    let lane = if accepts_opus && self.daemon_audio.load(Ordering::SeqCst) {
                        // Same takeover rule as the video lane: busy only
                        // while the holder is still an *active* route.
                        let holder = self.audio_lane_out.lock().get(&canon).cloned();
                        let holder_active = holder.as_deref().is_some_and(|rid| {
                            rid != route.id
                                && self
                                    .state
                                    .lock()
                                    .session
                                    .as_ref()
                                    .and_then(|s| s.route(rid))
                                    .is_some_and(|r| r.is_active())
                        });
                        if holder_active {
                            tracing::info!(
                                "route {} — peer's audio lane busy; falling back to PCM frames",
                                route.id
                            );
                            false
                        } else {
                            self.audio_lane_out.lock().insert(canon, route.id.clone());
                            true
                        }
                    } else {
                        false
                    };
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
                // We sink: play inbound frames for this route — and if we
                // asked for the Opus lane, claim this peer's inbound lane
                // so its `audio_inbound` frames decode into this route's
                // ring (the sender may still pick PCM, in which case the
                // claim simply never sees a frame).
                if to_node == me {
                    tracing::info!(
                        "route {} active — playing audio from {}",
                        route.id,
                        short_id(&from_node)
                    );
                    let offered_opus = self
                        .state
                        .lock()
                        .session
                        .as_ref()
                        .and_then(|s| s.route(&route.id))
                        .map(|r| r.audio.iter().any(|a| a == "opus"))
                        .unwrap_or(false);
                    if offered_opus {
                        self.audio_lane_in
                            .lock()
                            .insert(pubkey_part(&from_node).to_string(), route.id.clone());
                    }
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
                    let accepts_h264 = self
                        .state
                        .lock()
                        .session
                        .as_ref()
                        .and_then(|s| s.route(&route.id))
                        .map(|r| r.video.iter().any(|v| v == "h264"))
                        .unwrap_or(false);
                    let canon = pubkey_part(&to_node).to_string();
                    let daemon_video = self.daemon_video.load(Ordering::SeqCst);
                    if accepts_h264 && !daemon_video {
                        tracing::warn!(
                            "route {} — viewer accepts H.264 but the local daemon predates the track lane (needs myownmesh ≥ 0.2.1); streaming MJPEG",
                            route.id
                        );
                    }
                    let mode = if accepts_h264 && daemon_video {
                        // The lane is busy only while its holder is still
                        // an *active* route — a torn-down or superseded
                        // one (the common case: the viewer switched
                        // console tabs) is taken over, not deferred to.
                        let holder = self.video_lane_out.lock().get(&canon).cloned();
                        let holder_active = holder.as_deref().is_some_and(|rid| {
                            rid != route.id
                                && self
                                    .state
                                    .lock()
                                    .session
                                    .as_ref()
                                    .and_then(|s| s.route(rid))
                                    .is_some_and(|r| r.is_active())
                        });
                        if holder_active {
                            tracing::info!(
                                "route {} — peer's track lane busy; falling back to MJPEG",
                                route.id
                            );
                            VideoMode::Mjpeg
                        } else {
                            // Routine on every console tab switch — lane
                            // bookkeeping, not an event.
                            if let Some(h) = holder.filter(|h| h != &route.id) {
                                tracing::debug!(
                                    "route {} takes the track lane over from ended route {h}",
                                    route.id
                                );
                            }
                            self.video_lane_out
                                .lock()
                                .insert(canon.clone(), route.id.clone());
                            VideoMode::H264
                        }
                    } else {
                        VideoMode::Mjpeg
                    };
                    // Which monitor: the synthetic `screen` is the primary;
                    // a `screen:<id>` capability names one of the others
                    // (the ids come from this machine's own monitor
                    // enumeration — see `video::extra_screens`).
                    let monitor = route
                        .from
                        .as_str()
                        .split_once(':')
                        .and_then(|(_, dev)| dev.strip_prefix("screen:"))
                        .and_then(|id| id.parse::<u32>().ok());
                    tracing::info!(
                        "route {} active — streaming this {} to {} ({})",
                        route.id,
                        match monitor {
                            Some(id) => format!("monitor {id}"),
                            None => "screen".to_string(),
                        },
                        short_id(&to_node),
                        match mode {
                            VideoMode::H264 => "H.264 track",
                            VideoMode::Mjpeg => "MJPEG",
                        }
                    );
                    let peer = to_node.clone();
                    let tx = self.video_out.clone();
                    let status_mesh = Arc::downgrade(self);
                    let status_peer = to_node.clone();
                    let status_route = route.id.clone();
                    self.video.start_capture(
                        route.id.clone(),
                        mode,
                        monitor,
                        move |packet| {
                            // try_send: a full queue drops this packet; the next
                            // capture carries a fresher picture.
                            tx.try_send((peer.clone(), packet)).is_ok()
                        },
                        move |state, detail| {
                            // Capture-state transitions travel to the viewer
                            // in-band (`vstat`), so its console can explain a
                            // black stage instead of just showing one.
                            let Some(mesh) = status_mesh.upgrade() else {
                                return;
                            };
                            let frame = VideoStatusFrame::new(status_route.clone(), state, detail);
                            let peer = status_peer.clone();
                            tauri::async_runtime::spawn(async move {
                                let Ok(payload) = serde_json::to_value(&frame) else {
                                    return;
                                };
                                if let Err(e) = mesh.send_media_value(&peer, payload).await {
                                    tracing::debug!(
                                        "capture status to {} failed: {e}",
                                        short_id(&peer)
                                    );
                                }
                            });
                        },
                    );
                } else if to_node == me {
                    // Claim the peer's inbound lane for this route if we
                    // offered H.264 — the sender may still pick MJPEG, in
                    // which case the claim simply never sees a sample.
                    let offered_h264 = self
                        .state
                        .lock()
                        .session
                        .as_ref()
                        .and_then(|s| s.route(&route.id))
                        .map(|r| r.video.iter().any(|v| v == "h264"))
                        .unwrap_or(false);
                    if offered_h264 {
                        self.video_lane_in
                            .lock()
                            .insert(pubkey_part(&from_node).to_string(), route.id.clone());
                    }
                    // The "first screen frame" line is the signal worth a
                    // default-level entry on this side; this one only
                    // matters when that line never follows.
                    tracing::debug!(
                        "route {} active — expecting screen frames from {}",
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
            MediaKind::Generic if is_terminal_route(route) => {
                if from_node == me && to_node != me {
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
            other => {
                tracing::info!(
                    "route {} active ({other:?}); media transport for it is a follow-up",
                    route.id
                );
            }
        }
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
            tauri::async_runtime::spawn(async move {
                let _ = mesh.disconnect(rid).await;
            });
            return;
        }
        match self.terminal.spawn(&rid) {
            Ok(mut out_rx) => {
                tracing::info!(
                    "route {rid} active — hosting a terminal for {}",
                    short_id(&peer)
                );
                let mesh = self.clone();
                tauri::async_runtime::spawn(async move {
                    let mut seq: u64 = 0;
                    let mut last_ok = std::time::Instant::now();
                    let mut last_warn = std::time::Instant::now() - WARN_EVERY;
                    while let Some(msg) = out_rx.recv().await {
                        match msg {
                            OutMsg::Data(bytes) => {
                                for frame in
                                    TermFrame::data_frames(&rid, seq, &bytes, MAX_TERM_DATA_BYTES)
                                {
                                    seq = frame.seq + 1;
                                    let Ok(payload) = serde_json::to_value(&frame) else {
                                        continue;
                                    };
                                    match mesh.send_media_value(&peer, payload).await {
                                        Ok(()) => last_ok = std::time::Instant::now(),
                                        Err(e) => {
                                            if last_warn.elapsed() >= WARN_EVERY {
                                                last_warn = std::time::Instant::now();
                                                tracing::warn!(
                                                    "terminal output to {} failed: {e}",
                                                    short_id(&peer)
                                                );
                                            }
                                            // Nothing else reaps a session
                                            // whose viewer silently vanished
                                            // (peer drops never reach the
                                            // session) — the pump is the
                                            // watchdog.
                                            if last_ok.elapsed() > TERM_SEND_PATIENCE {
                                                tracing::warn!(
                                                    "terminal {rid} — viewer unreachable; ending the session"
                                                );
                                                let _ = mesh.disconnect(rid.clone()).await;
                                                return;
                                            }
                                        }
                                    }
                                }
                            }
                            OutMsg::Exit(code) => {
                                tracing::info!("terminal {rid} — shell ended ({code:?})");
                                let frame = TermFrame::new(&rid, seq, TermEvent::Exit { code });
                                if let Ok(payload) = serde_json::to_value(&frame) {
                                    let _ = mesh.send_media_value(&peer, payload).await;
                                }
                                let _ = mesh.disconnect(rid.clone()).await;
                                return;
                            }
                        }
                    }
                    // Stream closed without an Exit: `stop` ran, meaning a
                    // teardown is already in motion — nothing left to do.
                });
            }
            Err(e) => {
                // Tell the viewer in its own terms — a terminal renders a
                // line of text better than a silently vanished route — then
                // tear the route down.
                tracing::warn!("route {rid} — shell didn't start: {e}");
                let mesh = self.clone();
                tauri::async_runtime::spawn(async move {
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
            }
        } else if views_here {
            match frame.event {
                TermEvent::Data { bytes } => {
                    if self.terminal.enqueue(&frame.route, bytes) {
                        // Queue went empty → non-empty: poke the window to
                        // drain (a lost poke costs latency, never bytes —
                        // the safety poll catches up).
                        let _ = self.app.emit("allmystuff://term-ready", &frame.route);
                    }
                }
                TermEvent::Exit { code } => {
                    let _ = self.app.emit(
                        "allmystuff://term-exit",
                        json!({ "route": frame.route, "code": code }),
                    );
                }
                TermEvent::Resize { .. } => {}
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
        let peer = {
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
            r.peer.to_string()
        };
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
        let (hosts_here, views_here) = {
            let st = self.state.lock();
            let Some(r) = st.session.as_ref().and_then(|s| s.route(&frame.route)) else {
                return;
            };
            if !(r.is_active()
                && is_files_route(&r.route)
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
            )
        };
        if hosts_here {
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
                // Response kinds landing on the host are a confused peer.
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
                let _ = self.app.emit("allmystuff://file-ready", &frame.route);
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
        tauri::async_runtime::spawn(async move {
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
        tauri::async_runtime::spawn(async move {
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
        match event {
            FileEvent::List { .. }
            | FileEvent::Read { .. }
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
            if !(r.is_active() && is_files_route(&r.route) && node_of(r.route.to.as_str()) == me) {
                return Err("route isn't an active files session here".into());
            }
            r.peer.to_string()
        };
        let seq = self.file_seq.fetch_add(1, Ordering::Relaxed);
        let frame = FileFrame::new(&route_id, seq, event);
        let payload = serde_json::to_value(&frame).map_err(|e| e.to_string())?;
        self.send_media_value(&peer, payload).await
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
            let _ = self.app.emit(
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
            let _ = self.app.emit(
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
            let _ = self.app.emit(
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
        let _ = self.app.emit(
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

    /// Whether `sender` may drive this machine's keyboard and mouse: it is
    /// the recorded owner, or a member of the owned fleet this device
    /// belongs to. Nobody else — not even a peer a route auto-accepted for.
    fn sender_may_control(&self, sender: &str) -> bool {
        let canon = pubkey_part(sender);
        if self.ownership.owner().as_deref().map(pubkey_part) == Some(canon) {
            return true;
        }
        self.ownership.fleet().is_some_and(|r| {
            r.members
                .iter()
                .any(|m| pubkey_part(m.device.as_str()) == canon)
        })
    }

    /// Ask the far end of an inbound display route for a clean decode
    /// entry (IDR) *now* — the decoder here lost its place. Rate-limited
    /// per route: decode errors arrive at frame rate, the asks must not.
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

    /// Ask the far end of an inbound display route to stream with these
    /// quality picks (`None` = that dial back on automatic). Old peers
    /// drop the message and stay on automatic.
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
        let _ = self.app.emit("allmystuff://session", self.snapshot());
    }

    fn emit_status(&self, status: &str, error: Option<&str>) {
        let _ = self.app.emit(
            "allmystuff://subscription",
            json!({ "status": status, "error": error }),
        );
    }
}

/// A well-formed but empty owned roster (no fleet yet).
fn empty_owned() -> Value {
    json!({ "key": "", "version": 0, "members": [] })
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

fn parse_media(s: &str) -> MediaKind {
    match s {
        "audio" => MediaKind::Audio,
        "video" => MediaKind::Video,
        "display" => MediaKind::Display,
        "input" => MediaKind::Input,
        "storage" => MediaKind::Storage,
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
