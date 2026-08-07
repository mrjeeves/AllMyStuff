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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use crate::UiSink;

use allmystuff_graph::{Grant, MediaKind, NodeId, Person, PersonId, Route};
use allmystuff_protocol::control::{InboundFrame, MEDIA_KIND_AUDIO, MEDIA_KIND_VIDEO};
use allmystuff_protocol::{
    claim_code_network_id, format_claim_code, AppControl, ClientId, ControlMessage,
    DriveRouteOffer, KvmControl, NodeProfile, OwnedMember, OwnedRoster, OwnershipControl, Request,
    RoomMessage, RouteControl, ShareControl, SharedFileMeta, SiteControl, SiteService,
    TerminalSessionInfo, CHANNEL_CONTROL, CHANNEL_MEDIA, CHANNEL_PRESENCE, CHANNEL_ROOMS,
    LOCAL_CLAIM_NETWORK_ID, PROTOCOL_VERSION,
};
use allmystuff_session::{
    AudioFrame, ClipboardContentKind, ClipboardEvent, ClipboardFrame, ClipboardItem, Effect,
    FileEvent, FileFrame, InputAction, InputEvent, MediaPayload, RouteState, Session, SiteEvent,
    SiteFrame, TermEvent, TermFrame, VideoAssembler, VideoFrame, VideoStatusFrame,
    CLIPBOARD_CHUNK_BYTES, SITE_CHUNK_BYTES,
};

use crate::audio::{AudioBridge, CaptureSource};
use crate::clipboard::{ClipboardService, LocalClip};
use crate::control_client::{ControlClient, MediaPipe, MediaTrackPipe};
use crate::drive_mount::DriveMounts;
use crate::files::FilesPlane;
use crate::input_inject::Injector;
use crate::ownership::Ownership;
use crate::shares::Shares;
use crate::sites::{ClientMapping, SitesProxy};
use crate::terminal::{OutMsg, TerminalHost};
use crate::video::{VideoBridge, VideoMode, VideoPacket, VideoSource};
use crate::video_decode::{Au, DecodeBridge, DecoderPreference};
use std::time::{Duration, Instant};

pub struct Mesh {
    client: Arc<ControlClient>,
    /// The media plane's dedicated daemon connection: frame chunks ride it
    /// back-to-back instead of paying a connect + round trip each.
    media_pipe: MediaPipe,
    /// The binary lane for H.264/Opus track sends (no base64); MJPEG, PCM and
    /// route signalling stay on `media_pipe`.
    media_track_pipe: MediaTrackPipe,
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
    /// M2 — the pacer's requested-vs-actual gap ledger (one log line per
    /// minute): the honesty check on every sub-millisecond spacing the
    /// drain model asks for.
    pace_gaps: Mutex<PaceGapStats>,
    /// M3 + the chunk-train bandwidth estimator: per inbound video route,
    /// arrival dispersion of the pacer's own timed bursts → a bottleneck
    /// estimate and a one-way-delay trend, attached to every outbound
    /// [`RouteControl::VideoFeedback`] (the ICE datapath's control
    /// channel — never signaling).
    video_arrivals: Mutex<HashMap<String, ArrivalState>>,
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
    /// OS-native drive letters/mounts backed by active Storage routes.
    drive_mounts: DriveMounts,
    /// Explicit inbound pull requests. A drive pushed at us needs owner/fleet
    /// trust or a live CEC Control grant; this token is what lets a Files
    /// share/support source answer a pull in the opposite direction.
    drive_pull_tokens: Mutex<HashMap<String, DrivePullRequest>>,
    /// Successful receiver-initiated mappings, keyed by their current route.
    /// The route is one connection incarnation; this is the user's intent and
    /// survives either app restarting and a source laptop sleeping.
    drive_reconnects: Mutex<HashMap<String, DriveReconnect>>,
    /// Crash-safe backing store for the receiver-owned mapping intents.
    drive_reconnect_path: Option<PathBuf>,
    /// Old route ids currently being rebuilt, so repeated presence adverts do
    /// not launch duplicate offers for the same native drive.
    drive_reconnect_inflight: Mutex<std::collections::HashSet<String>>,
    /// Remote KVM-media operations awaiting source acknowledgement and final
    /// completion. Kept viewer-side so a peer cannot spoof a result without
    /// both the unguessable request id and the expected source identity.
    kvm_media_requests: Mutex<HashMap<String, KvmMediaRequest>>,
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
    /// Client mappings currently being auto-re-mapped after a reject (keyed
    /// `<pubkey>:<host_port>`), so a burst of rejects can't spawn a stampede of
    /// competing heal tasks for the same tunnel.
    site_remap_inflight: Mutex<std::collections::HashSet<String>>,
    /// Per-route rate limit for the dead-site-route NACK (last send `Instant`),
    /// so a peer draining a full pipe onto a route we no longer hold gets one
    /// Reject, not one per frame.
    site_nack_at: Mutex<HashMap<String, std::time::Instant>>,
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
    /// Canonical pubkeys of devices THIS node has sent an ownership `Claim` to
    /// and is awaiting a `Claimed` confirmation from. An inbound
    /// `OwnershipControl::Claimed` is honoured only when its authenticated
    /// sender is in this set — and the entry is consumed on use — so an
    /// *unsolicited* `Claimed` from an arbitrary peer can't drive itself into
    /// this device's fleet member list and signed roster (which
    /// [`Mesh::sender_may_control`] trusts), i.e. can't hand itself control of
    /// this machine. The outbound-claim mirror of the per-sender guards the
    /// other ownership arms already apply. In-memory only: a claim interrupted
    /// by a restart simply needs re-issuing.
    pending_claims: Mutex<std::collections::HashSet<String>>,
    /// Latest passive clock-skew sample per peer (ms; positive = the peer's
    /// wall clock reads ahead of ours) with when it landed, from the
    /// `sent_at` stamp presence adverts carry. Fed to the network verdict in
    /// [`Mesh::note_peer_clock`]; stale entries age out of the vote rather
    /// than an offline peer's old clock voting forever.
    peer_clock_skew: Mutex<HashMap<String, (i64, std::time::Instant)>>,
    /// Whether the "this device's clock is out of sync" warning is currently
    /// raised — latched so it fires once per episode (and clears once), not
    /// on every presence advert while the clock stays wrong.
    clock_skew_warned: std::sync::atomic::AtomicBool,
    /// When each outbound route offer was first seen still-unanswered by the
    /// reaper sweep ([`Mesh::spawn_offer_reaper`]). An offer has no deadline
    /// in the wire protocol and the session is clock-free, so this is where
    /// "awaiting accept" gets its timer; entries leave when the route stops
    /// being an outbound `Offered`.
    offer_first_seen: Mutex<HashMap<String, std::time::Instant>>,
    /// The daemon-link status as last emitted on `allmystuff://subscription`
    /// — answered back by [`Mesh::mesh_status`], because the emit itself is
    /// one-shot and a late-subscribing GUI misses it.
    last_status: Mutex<(String, Option<String>)>,
    /// Last non-empty fleet roster we read from the closed network's signed
    /// roster (`fleet_roster_value`). A member-side resilience cache — the
    /// symmetric twin of the owner's durable `fleet_members()` fallback: the
    /// signed roster is the source of truth, but it's momentarily unreadable
    /// while the fleet's closed network is mid-(re)join, and during that gap a
    /// co-member must not flicker to "another fleet". A non-empty read always
    /// replaces this, so an eviction propagates the instant the roster is
    /// readable again — we never resurrect a removed member.
    fleet_roster_cache: Mutex<Vec<OwnedMember>>,
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
    video_out: mpsc::Sender<VideoOut>,
    /// The matching receivers, parked by [`Mesh::new`] and drained by the
    /// forwarder tasks [`Mesh::start`] spawns. They live here rather than
    /// being spawned in `new` because the GUI builds the `Mesh` in a
    /// *synchronous* Tauri `setup` (no ambient Tokio runtime to spawn on);
    /// `start` is the first point guaranteed an async context, and on the
    /// same runtime everything else runs on.
    audio_rx: Mutex<Option<mpsc::Receiver<AudioOut>>>,
    video_rx: Mutex<Option<mpsc::Receiver<VideoOut>>>,
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
    /// Our presence boot id — how peers detect that we (re)started and answer
    /// with their state (see `NodeProfile::boot`). Seeded once per app run, but
    /// **refreshed whenever a local network reset drops our peer caches** (see
    /// [`Mesh::prune_unjoined_peers`]): the reset discards everything we knew
    /// about each peer, so we are a fresh incarnation as far as their state is
    /// concerned, and a new boot id is exactly what makes them re-send it.
    /// Without the bump, a network refresh on one side left the *other* side
    /// (same boot id, peer still "known") silent, stranding the connection
    /// until both sides refreshed or an app restarted.
    boot_id: AtomicU64,
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
    /// When each inbound track lane was first seen carrying media that no
    /// route here maps to (key `deadlane:<media>:<peer>:<lane>`), cleared
    /// the moment the lane resolves. A lane-shaped NACK
    /// ([`RouteControl::DeadLane`]) is sent only once the condition has
    /// persisted a full [`WARN_EVERY`] — a stream's first samples can
    /// legally outrun the Accept/VideoLane control messages at start, and
    /// NACKing that instant would kill a healthy stream being born.
    dead_lane_since: Mutex<HashMap<String, std::time::Instant>>,
    /// When each route last asked its sender for a clean decode entry —
    /// decode errors arrive at frame rate; the asks must not.
    refresh_asks: Mutex<HashMap<String, std::time::Instant>>,
    /// Per-peer backoff state for the refresh round-trip ([`ControlMessage::
    /// ProfileRequest`]), so a held-down refresh can't hammer a peer. See
    /// [`Mesh::allow_profile_request`].
    profile_req: Mutex<HashMap<String, ProfileReqState>>,
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
    /// Whether the local daemon speaks the **binary media pipes**
    /// (`media_track_pipe` / `media_source_pipe`, from Status `media_pipes`).
    /// The version pin can't gate this — the feature predates a release — so
    /// it's a capability flag. While false, H.264/Opus ride the legacy base64
    /// `video_send`/`audio_send` ops and inbound arrives as base64 events, so
    /// an older daemon on the socket still streams (just with the base64 tax)
    /// instead of a black screen.
    daemon_media_pipes: std::sync::atomic::AtomicBool,
    /// **Host side:** the RTP video track lane pinned to each route we
    /// stream, by route id. Assigned once (lowest free in the peer's pool)
    /// when the stream starts and held until teardown, so an unrelated route
    /// coming or going never renumbers a live stream's lane. The viewer is
    /// told the binding ([`RouteControl::VideoLane`]) and demuxes by it.
    video_lane_pins: Mutex<HashMap<String, u8>>,
    /// Process-local incarnation of each outbound video route. Route ids are
    /// intentionally stable across a rapid codec/source re-offer, so the id
    /// alone cannot tell a queued AU from the capture instance that produced
    /// it. The generation is never serialized: it only fences stale callbacks
    /// and queued work before they reach the existing media plane.
    video_route_generations: Mutex<VideoRouteGenerations>,
    /// A very small, process-local guard around a screen switch. The viewer
    /// tears the old display route down and offers the new one on separate
    /// local node-control requests; a delayed duplicate teardown can therefore
    /// land just after the successor activates. Route ids carry no incarnation,
    /// so without this narrow fence that close tears down the brand-new
    /// monitor and changing codec merely happens to start it again after the
    /// race. Local duplicates are watch-confirmed inside 100 ms; inbound ones
    /// wait a bounded 2.5 seconds for an existing ICE-path liveness control.
    /// Nothing here is serialized and no new message is sent over any channel.
    video_switch_guards: Mutex<VideoSwitchGuards>,
    /// **Viewer side:** the lane→route binding a streamer told us, per peer
    /// (canonical pubkey). Inbound H.264 on lane `L` from peer `P` belongs to
    /// `video_lane_binds[P][L]` — authoritative over the positional guess.
    /// Empty for a peer that doesn't announce (older build): that peer's lanes
    /// fall back to the positional sort.
    video_lane_binds: Mutex<HashMap<String, HashMap<u8, String>>>,
    /// The disabled-networks park store, when the embedding process shares
    /// one (the node binary's `network_set_enabled` seam). Consulted by
    /// [`Mesh::ensure_claim_networks`] so a deliberately switched-off local
    /// claim network *stays* off across claim-state changes instead of
    /// being silently re-joined — the network can't be left, so the park
    /// store is the only "off" it has, and it has to stick.
    disabled_networks: Mutex<Option<Arc<crate::networks_store::DisabledNetworks>>>,
    /// CEC Support state — the technician's dialed customers + Agent Name, and
    /// the customer's consent store + pending connect-requests. Empty and inert
    /// on a node that never joins the CEC ecosystem; when it does, its per-frame
    /// gate ([`Mesh::sender_may_drive`]) additively consults the consent store
    /// so a dialed technician's screen/input rides the very same engine, trusted
    /// by a live grant instead of owner/fleet. See [`crate::cec`].
    cec: crate::cec::Cec,
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
    /// Which native H.264 rung this local window selected. This never leaves
    /// the GUI-to-node process boundary.
    decoder: DecoderPreference,
    queue: std::collections::VecDeque<Vec<u8>>,
    /// Updated by the window's 16 ms safety poll even when no frame arrived.
    /// A post-disconnect-request poll is stronger liveness evidence than mere
    /// watcher presence because `video_unwatch` is fire-and-forget.
    last_poll: Instant,
}

struct KvmMediaRequest {
    source: String,
    kvm: String,
    label: String,
    made: Instant,
    acknowledged: Option<oneshot::Sender<()>>,
}

/// A receiver-initiated native-drive pull. The request token is also the
/// authorization proof for the source's inbound offer; retaining the user's
/// selections lets the receiver rebuild the mapping when that source reports
/// a fresh incarnation after sleep or an app restart.
#[derive(Clone)]
struct DrivePullRequest {
    source: String,
    root: String,
    label: String,
    mount: String,
    made: Instant,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct DriveReconnect {
    source: String,
    root: String,
    label: String,
    mount: String,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct PersistedDriveReconnects {
    #[serde(default)]
    mappings: Vec<DriveReconnect>,
}

fn drive_reconnect_store_path() -> Option<PathBuf> {
    let home = std::env::var_os("MYOWNMESH_HOME")
        .map(PathBuf::from)
        .or_else(dirs::home_dir)?;
    Some(home.join(".myownmesh").join("allmystuff-drives.json"))
}

fn load_drive_reconnects(path: &Option<PathBuf>) -> HashMap<String, DriveReconnect> {
    let persisted: PersistedDriveReconnects = path
        .as_ref()
        .map(|path| crate::persist::load_json(path))
        .unwrap_or_default();
    persisted
        .mappings
        .into_iter()
        .enumerate()
        .map(|(index, mapping)| (format!("saved:{index}"), mapping))
        .collect()
}

fn persist_drive_reconnects(
    path: &Option<PathBuf>,
    mappings: &HashMap<String, DriveReconnect>,
) -> bool {
    let Some(path) = path else { return true };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut durable = Vec::new();
    for mapping in mappings.values() {
        if !durable.contains(mapping) {
            durable.push(mapping.clone());
        }
    }
    durable.sort_by(|a, b| {
        (&a.source, &a.root, &a.mount, &a.label).cmp(&(&b.source, &b.root, &b.mount, &b.label))
    });
    let persisted = PersistedDriveReconnects { mappings: durable };
    match serde_json::to_string_pretty(&persisted) {
        Ok(json) => crate::persist::write_atomic(path, json.as_bytes()).is_ok(),
        Err(_) => false,
    }
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

/// One item on the shipped shared video queue. Generation and recovery
/// metadata are strictly process-local: the packet still reaches the same
/// established media sender with the same bytes and duration as before.
type VideoOut = (String, String, u64, VideoPacket, u64, Arc<VideoRecovery>);

#[derive(Default)]
struct VideoRouteGenerations {
    next: u64,
    current: HashMap<String, u64>,
}

impl VideoRouteGenerations {
    fn begin(&mut self, route_id: &str) -> (u64, Option<u64>) {
        self.next = self.next.wrapping_add(1).max(1);
        let generation = self.next;
        let replaced = self.current.insert(route_id.to_string(), generation);
        (generation, replaced)
    }

    fn retire(&mut self, route_id: &str) {
        self.current.remove(route_id);
    }

    fn current(&self, route_id: &str) -> Option<u64> {
        self.current.get(route_id).copied()
    }

    fn is_current(&self, route_id: &str, generation: u64) -> bool {
        self.current
            .get(route_id)
            .is_some_and(|current| *current == generation)
    }
}

/// A predecessor remains eligible to arm a switch guard for this long. The
/// real switch is normally a few milliseconds; the wider retention only makes
/// the bookkeeping tolerant of a loaded viewer. It does not widen the actual
/// teardown-ignore window below.
const VIDEO_SWITCH_PREDECESSOR_AGE: Duration = Duration::from_secs(2);
/// Fence closes this soon after a display-switch successor starts. 100 ms is
/// well above the 7 ms field failure while remaining a narrow intent check.
/// Every duplicate inside the same window is fenced; none consumes the guard.
const VIDEO_SWITCH_TEARDOWN_GUARD: Duration = Duration::from_millis(100);
/// A poll that was already in flight when disconnect began is not proof. The
/// window's safety loop runs every 16 ms, so require a poll at least two ticks
/// later and observe for long enough that an active loop can produce one.
const VIDEO_LOCAL_POLL_PROOF_MIN_AGE: Duration = Duration::from_millis(32);
const VIDEO_LOCAL_POLL_OBSERVE: Duration = Duration::from_millis(80);
/// A first close that races a just-started display successor waits briefly for
/// proof that the replacement is alive. Viewer feedback is emitted at most two
/// seconds apart, so 2.5 seconds covers one full beat without letting a genuine
/// one-shot close strand an encoder indefinitely.
const VIDEO_INBOUND_TEARDOWN_QUARANTINE: Duration = Duration::from_millis(2_500);
/// Ignore an immediate, possibly already-in-flight feedback beat. A periodic
/// viewer report that arrives after this floor is evidence produced by the
/// replacement route, not merely setup traffic queued beside its offer.
const VIDEO_TEARDOWN_LIVENESS_MIN_AGE: Duration = Duration::from_millis(250);
/// Lifecycle entries outlive the 2.5-second quarantine but are pruned during
/// later route activity, bounding the bookkeeping on long-running nodes.
const VIDEO_SWITCH_BOOK_RETENTION: Duration = Duration::from_secs(10);

struct StoppedVideoRoute {
    peer: String,
    sink: String,
    at: Instant,
}

struct StartedVideoRoute {
    peer: String,
    /// The recent route whose stop made this start a display switch. It remains
    /// readable for the whole narrow guard window so duplicate local/backend
    /// calls cannot defeat the fence merely by racing one another.
    predecessor: Option<String>,
    at: Instant,
    incarnation: u64,
}

struct PendingVideoTeardown {
    token: u64,
    armed_at: Instant,
    incarnation: u64,
}

#[derive(Default)]
struct VideoSwitchGuards {
    stopped: HashMap<String, StoppedVideoRoute>,
    started: HashMap<String, StartedVideoRoute>,
    /// Early inbound teardown quarantines, route → opaque local token. An
    /// mature periodic viewer report cancels the token; duplicate closes
    /// coalesce behind the same bounded timer.
    pending: HashMap<String, PendingVideoTeardown>,
    next_pending: u64,
    next_incarnation: u64,
}

struct VideoSwitchGuardHit {
    predecessor: String,
    age: Duration,
    incarnation: u64,
}

enum InboundVideoTeardownGate {
    Commit,
    CoalesceDuplicate {
        token: u64,
    },
    Quarantine {
        predecessor: String,
        age: Duration,
        token: u64,
        incarnation: u64,
    },
}

impl VideoSwitchGuards {
    fn note_stop(&mut self, route_id: &str, peer: &str, sink: &str, now: Instant) {
        self.started.remove(route_id);
        self.pending.remove(route_id);
        self.started.retain(|_, start| {
            now.saturating_duration_since(start.at) <= VIDEO_SWITCH_BOOK_RETENTION
        });
        self.stopped.retain(|_, stop| {
            now.saturating_duration_since(stop.at) <= VIDEO_SWITCH_PREDECESSOR_AGE
        });
        self.stopped.insert(
            route_id.to_string(),
            StoppedVideoRoute {
                peer: pubkey_part(peer).to_string(),
                sink: sink.to_string(),
                at: now,
            },
        );
    }

    fn note_start(&mut self, route_id: &str, peer: &str, sink: &str, now: Instant) {
        // A real re-offer supersedes any old delayed-close timer for this
        // deterministic route id.
        self.pending.remove(route_id);
        self.started.retain(|_, start| {
            now.saturating_duration_since(start.at) <= VIDEO_SWITCH_BOOK_RETENTION
        });
        self.stopped.retain(|_, stop| {
            now.saturating_duration_since(stop.at) <= VIDEO_SWITCH_PREDECESSOR_AGE
        });
        let peer = pubkey_part(peer).to_string();
        // Prefer the newest matching predecessor. The same-id case is a codec
        // re-offer; a different id with the same sink is a monitor switch.
        let predecessor = self
            .stopped
            .iter()
            .filter(|(_, stop)| stop.peer == peer && stop.sink == sink)
            .max_by_key(|(_, stop)| stop.at)
            .map(|(id, _)| id.clone());
        self.next_incarnation = self.next_incarnation.wrapping_add(1).max(1);
        let incarnation = self.next_incarnation;
        self.started.insert(
            route_id.to_string(),
            StartedVideoRoute {
                peer,
                predecessor,
                at: now,
                incarnation,
            },
        );
    }

    fn take_early_teardown(
        &mut self,
        route_id: &str,
        peer: &str,
        now: Instant,
    ) -> Option<VideoSwitchGuardHit> {
        let start = self.started.get_mut(route_id)?;
        if start.peer != pubkey_part(peer) {
            return None;
        }
        let age = now.saturating_duration_since(start.at);
        if age > VIDEO_SWITCH_TEARDOWN_GUARD {
            return None;
        }
        let predecessor = start.predecessor.clone()?;
        Some(VideoSwitchGuardHit {
            predecessor,
            age,
            incarnation: start.incarnation,
        })
    }

    fn gate_inbound_teardown(
        &mut self,
        route_id: &str,
        peer: &str,
        now: Instant,
    ) -> InboundVideoTeardownGate {
        if let Some(pending) = self.pending.get(route_id) {
            return InboundVideoTeardownGate::CoalesceDuplicate {
                token: pending.token,
            };
        }
        let Some(hit) = self.take_early_teardown(route_id, peer, now) else {
            return InboundVideoTeardownGate::Commit;
        };
        let token = self.arm_pending(route_id, hit.incarnation, now);
        InboundVideoTeardownGate::Quarantine {
            predecessor: hit.predecessor,
            age: hit.age,
            token,
            incarnation: hit.incarnation,
        }
    }

    fn arm_pending(&mut self, route_id: &str, incarnation: u64, now: Instant) -> u64 {
        self.next_pending = self.next_pending.wrapping_add(1).max(1);
        let token = self.next_pending;
        self.pending.insert(
            route_id.to_string(),
            PendingVideoTeardown {
                token,
                armed_at: now,
                incarnation,
            },
        );
        token
    }

    fn cancel_pending(&mut self, route_id: &str) -> Option<u64> {
        self.pending.remove(route_id).map(|pending| pending.token)
    }

    fn cancel_pending_on_mature_liveness(&mut self, route_id: &str, now: Instant) -> Option<u64> {
        let pending = self.pending.get(route_id)?;
        if now.saturating_duration_since(pending.armed_at) < VIDEO_TEARDOWN_LIVENESS_MIN_AGE {
            return None;
        }
        self.cancel_pending(route_id)
    }

    fn take_pending_if_current(&mut self, route_id: &str, token: u64, incarnation: u64) -> bool {
        let pending_matches = self
            .pending
            .get(route_id)
            .is_some_and(|pending| pending.token == token && pending.incarnation == incarnation);
        let incarnation_matches = self
            .started
            .get(route_id)
            .is_some_and(|started| started.incarnation == incarnation);
        if !pending_matches || !incarnation_matches {
            return false;
        }
        self.pending.remove(route_id);
        true
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InboundVideoDisposition {
    Accept,
    /// The authenticated peer is sending for the correct display/video route,
    /// but its first media beat outran Accept. Drop it quietly: rejecting a
    /// stable route id here would tear down the same-id successor.
    Pending,
    Reject,
}

fn inbound_video_disposition_from_facts(
    state: Option<&RouteState>,
    video_media: bool,
    sinks_here: bool,
    sender_is_peer: bool,
) -> InboundVideoDisposition {
    if !video_media || !sinks_here || !sender_is_peer {
        return InboundVideoDisposition::Reject;
    }
    match state {
        Some(RouteState::Active) => InboundVideoDisposition::Accept,
        Some(RouteState::Offered | RouteState::Incoming) => InboundVideoDisposition::Pending,
        _ => InboundVideoDisposition::Reject,
    }
}

/// The receiver's first sample must independently open a decoder reference
/// chain. The daemon's `key` bit recognizes H.264 IDRs, while HEVC/AV1 entry
/// AUs are identified by their parameter sets in the payload.
fn should_hold_first_video_sample(first: bool, key: bool, data: &[u8]) -> bool {
    first && !key && !crate::video_decode::is_decode_entry(data)
}

/// Queue-local recovery state shared by capture and its sender worker. The
/// epoch prevents an older keyframe from declaring recovery after a newer
/// drop: only a key produced in the current damage epoch and successfully
/// handed to the existing media pipe can release dependent deltas.
struct VideoRecovery {
    route_id: String,
    diag_key: String,
    /// `(epoch << 1) | awaiting_key`. Keeping both facts in one atomic makes a
    /// drop and a delivered-key decision indivisible: a stale key cannot land
    /// between separate epoch/awaiting writes and falsely release deltas.
    state: AtomicU64,
    drops: AtomicU64,
    suppressed: AtomicU64,
}

impl VideoRecovery {
    fn new(route_id: &str) -> Self {
        Self {
            route_id: route_id.to_string(),
            diag_key: format!("video-recovery:{route_id}"),
            state: AtomicU64::new(0),
            drops: AtomicU64::new(0),
            suppressed: AtomicU64::new(0),
        }
    }

    fn epoch(&self) -> u64 {
        self.state.load(Ordering::Acquire) >> 1
    }

    fn suppresses(&self, key: Option<bool>) -> bool {
        suppress_dependent_after_drop(self.state.load(Ordering::Acquire) & 1 != 0, key)
    }

    fn note_suppressed(&self, mesh: &Mesh) {
        let suppressed = self.suppressed.fetch_add(1, Ordering::Relaxed) + 1;
        if mesh.diag_ok(&self.diag_key) {
            tracing::warn!(
                "video queue recovery for {}: {} total drops, {suppressed} total dependent deltas suppressed; awaiting delivered IDR",
                self.route_id,
                self.drops.load(Ordering::Relaxed)
            );
        }
    }

    fn note_drop(&self, mesh: &Mesh, key: Option<bool>, reason: &str) {
        let (arm, dropped, _) = self.mark_drop(key);
        // The first loss starts recovery. A keyframe that itself fails must
        // re-arm it; suppressed deltas never do, avoiding an IDR storm.
        if arm {
            mesh.video.force_idr(&self.route_id);
        }
        if mesh.diag_ok(&self.diag_key) {
            tracing::warn!(
                "video queue recovery for {}: {dropped} total drops ({reason}); {}",
                self.route_id,
                if arm {
                    "IDR armed"
                } else {
                    "awaiting delivered IDR"
                }
            );
        }
    }

    /// Advance the damage epoch and enter recovery. Returns whether the
    /// encoder must be armed, the episode drop count, and the new epoch.
    fn mark_drop(&self, key: Option<bool>) -> (bool, u64, u64) {
        let mut old = self.state.load(Ordering::Acquire);
        let (arm, epoch) = loop {
            let was_awaiting = old & 1 != 0;
            // A dependent unit that raced into a send before recovery began
            // is covered by the repair already in flight. Advancing its epoch
            // would stale that repair without arming a replacement.
            if was_awaiting && key == Some(false) {
                break (false, old >> 1);
            }
            let next_epoch = (old >> 1).wrapping_add(1);
            let new = (next_epoch << 1) | 1;
            match self
                .state
                .compare_exchange_weak(old, new, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => break (!was_awaiting || key == Some(true), next_epoch),
                Err(actual) => old = actual,
            }
        };
        let dropped = self.drops.fetch_add(1, Ordering::Relaxed) + 1;
        (arm, dropped, epoch)
    }

    fn note_key_delivered(&self, packet_epoch: u64) -> bool {
        let recovering = (packet_epoch << 1) | 1;
        if self
            .state
            .compare_exchange(
                recovering,
                packet_epoch << 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return false;
        }
        let drops = self.drops.load(Ordering::Relaxed);
        let suppressed = self.suppressed.load(Ordering::Relaxed);
        tracing::info!(
            "video queue recovery for {}: IDR delivered (lifetime totals: {drops} drops, {suppressed} suppressed deltas)",
            self.route_id
        );
        true
    }
}

/// Auto-re-map after a site route is rejected: how many times to retry, and the
/// base backoff (grown by the attempt number). ~11s of retrying across 5 tries
/// — enough to ride out a KVM reconnect, few enough to give up (not loop) if the
/// host is genuinely refusing us.
const SITE_REMAP_ATTEMPTS: u32 = 5;
const SITE_REMAP_BACKOFF: std::time::Duration = std::time::Duration::from_millis(750);

/// Per-route cooldown for the dead-site-route NACK, so a client draining a full
/// pipe onto a route we no longer hold gets one Reject, not one per frame.
const SITE_NACK_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(30);

/// How long a CEC connect-request may wait for acknowledged delivery to
/// the customer's node. Covers the WebRTC bring-up plus a mid-dial
/// network wobble with room to spare; past it, the customer is genuinely
/// unreachable and the session honestly ends. (Delivery ≠ decision — the
/// customer can take as long as they like to click once the prompt is up.)
const CEC_CONNECT_TTL: std::time::Duration = std::time::Duration::from_secs(90);

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
    /// How each peer's nominated ICE pair actually flows (canonical pubkey →
    /// LAN/WAN), from the daemon's `PeersList` `selected_pair` — the LAN
    /// gate's signal for how generous the AUTOMATIC video dials may be.
    /// A peer with no reported pair (ICE unsettled, old daemon) simply isn't
    /// in the map: transient unknowns must never downgrade a learned class.
    peer_links: HashMap<String, crate::video::LinkClass>,
    /// Last presence boot id seen per peer (canonical pubkey). A boot id we
    /// haven't recorded means the peer just (re)started and missed our
    /// adverts — we answer with our state directly. This is what lets
    /// gossip be event-driven instead of a heartbeat.
    peer_boots: HashMap<String, u64>,
    client_id: Option<ClientId>,
    profile: Option<NodeProfile>,
}

/// M2 — the pacer's requested-vs-actual gap ledger (a minute at a time),
/// plus M1's daemon-write span (the pipe await per chunk).
#[derive(Default)]
struct PaceGapStats {
    n: u64,
    req_us: u64,
    act_us: u64,
    worst_over_us: u64,
    over_1ms: u64,
    write_us: u64,
    writes: u64,
    last_log: Option<Instant>,
}

/// M3 + T1.1 — one inbound video route's arrival measurement: the current
/// chunk train being timed, the dispersion-derived bandwidth estimate,
/// and the one-way-delay trend window.
struct ArrivalState {
    /// The train in progress (chunks sharing one RTP timestamp).
    ts: u32,
    first: Instant,
    last: Instant,
    bytes: usize,
    chunks: u32,
    /// EWMA of per-train dispersion samples (kbps); 0 = none yet. What a
    /// timed train measures is min(sender's drain rate, bottleneck) —
    /// exactly the number a closed loop can act on.
    est_kbps: f64,
    /// This minute's samples (Mbps) for the log line's percentiles.
    window: Vec<f64>,
    /// (arrival, relative one-way delay µs) over the last ~2 s — the
    /// slope is a standing queue growing before loss says so. Clock skew
    /// between the sender's RTP clock and our monotonic is ppm-scale,
    /// two orders under the trend threshold.
    owd: std::collections::VecDeque<(Instant, i64)>,
    /// Wall/RTP anchor for the relative delay; re-anchored periodically
    /// so u32 RTP wrap (~13 h) never crosses a window.
    base: Option<(Instant, u32)>,
    last_log: Instant,
}

/// Keep pacing inside the route's actual frame slot. The configured budget is
/// still the ceiling at ordinary frame rates; high-refresh streams get 90% of
/// one frame so pacing can never create a standing frame of sender latency.
fn pace_budget(configured_ms: u64, fps: u32) -> std::time::Duration {
    let configured_us = configured_ms.saturating_mul(1_000);
    let frame_us = 900_000 / u64::from(fps.max(1));
    std::time::Duration::from_micros(configured_us.min(frame_us).max(1))
}

/// Cap a requested gap to the AU's absolute pacing deadline. Pipe-lock and
/// write waits consume the same budget, so contention can shorten or remove a
/// later sleep but can never add a standing frame of deliberate pacing delay.
fn pace_gap_until(
    deadline: Instant,
    now: Instant,
    requested: std::time::Duration,
) -> std::time::Duration {
    requested.min(deadline.saturating_duration_since(now))
}

fn suppress_dependent_after_drop(awaiting_key: bool, key: Option<bool>) -> bool {
    awaiting_key && key == Some(false)
}

/// Execute one pacing gap against a deadline: bulk asynchronously (the
/// worker stays free for audio interleave), finish with the precise
/// sleeper so 100–1500 µs requests are real instead of timer-wheel
/// millisecond roundings. Falls back to the plain async sleep on a
/// current-thread runtime (tests), where blocking a worker would deadlock.
async fn paced_gap(gap: std::time::Duration) {
    let deadline = Instant::now() + gap;
    let precise_ok = tokio::runtime::Handle::try_current()
        .map(|h| h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
        .unwrap_or(false);
    loop {
        let now = Instant::now();
        let Some(rem) = deadline.checked_duration_since(now) else {
            return;
        };
        if rem > std::time::Duration::from_millis(3) {
            tokio::time::sleep(rem - std::time::Duration::from_millis(2)).await;
            continue;
        }
        if precise_ok {
            tokio::task::block_in_place(|| crate::os_perf::precise_sleep(rem));
        } else {
            tokio::time::sleep(rem).await;
        }
        return;
    }
}

impl Mesh {
    pub fn new(client: Arc<ControlClient>, sink: Arc<dyn UiSink>) -> Arc<Self> {
        // Shallow queues both: at most a few frames in flight, so a slow
        // link sheds load by dropping captures rather than growing latency.
        // Audio's 8 buffers are ~160 ms of slack.
        let (audio_out, audio_rx) = mpsc::channel::<AudioOut>(8);
        let (video_out, video_rx) = mpsc::channel::<VideoOut>(4);
        let drive_reconnect_path = drive_reconnect_store_path();
        let drive_reconnects = load_drive_reconnects(&drive_reconnect_path);
        Arc::new(Mesh {
            client: client.clone(),
            media_pipe: MediaPipe::new(client.clone()),
            media_track_pipe: MediaTrackPipe::new(client.clone()),
            sink,
            audio: Arc::new(AudioBridge::new()),
            video: Arc::new(VideoBridge::new()),
            video_decode: Arc::new(DecodeBridge::new()),
            pace_gaps: Mutex::new(PaceGapStats::default()),
            video_arrivals: Mutex::new(HashMap::new()),
            injector: Injector::new(),
            terminal: TerminalHost::new(),
            term_seq: AtomicU64::new(0),
            term_pumps: Mutex::new(std::collections::HashSet::new()),
            term_rx_seq: Mutex::new(HashMap::new()),
            term_in_seq: Mutex::new(HashMap::new()),
            files: FilesPlane::new(),
            drive_mounts: DriveMounts::new(),
            drive_pull_tokens: Mutex::new(HashMap::new()),
            drive_reconnects: Mutex::new(drive_reconnects),
            drive_reconnect_path,
            drive_reconnect_inflight: Mutex::new(std::collections::HashSet::new()),
            kvm_media_requests: Mutex::new(HashMap::new()),
            file_seq: AtomicU64::new(0),
            sites: SitesProxy::load(),
            site_seq: AtomicU64::new(0),
            site_remap_inflight: Mutex::new(std::collections::HashSet::new()),
            site_nack_at: Mutex::new(HashMap::new()),
            downloads: Mutex::new(HashMap::new()),
            shared: Mutex::new(HashMap::new()),
            state: Mutex::new(State {
                session: None,
                network: None,
                networks: Vec::new(),
                peer_networks: HashMap::new(),
                peer_features: HashMap::new(),
                peer_links: HashMap::new(),
                peer_boots: HashMap::new(),
                client_id: None,
                profile: None,
            }),
            ownership: Arc::new(Ownership::load()),
            fleet_authorized: Mutex::new(std::collections::HashSet::new()),
            pending_claims: Mutex::new(std::collections::HashSet::new()),
            peer_clock_skew: Mutex::new(HashMap::new()),
            clock_skew_warned: std::sync::atomic::AtomicBool::new(false),
            offer_first_seen: Mutex::new(HashMap::new()),
            last_status: Mutex::new(("unknown".into(), None)),
            fleet_roster_cache: Mutex::new(Vec::new()),
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
            boot_id: AtomicU64::new(fresh_boot_id()),
            video_in: Mutex::new(VideoAssembler::new()),
            video_watchers: Mutex::new(HashMap::new()),
            daemon_video: std::sync::atomic::AtomicBool::new(false),
            video_in_stats: Mutex::new(HashMap::new()),
            video_diag_last: Mutex::new(HashMap::new()),
            dead_lane_since: Mutex::new(HashMap::new()),
            refresh_asks: Mutex::new(HashMap::new()),
            profile_req: Mutex::new(HashMap::new()),
            audio_decoders: Mutex::new(HashMap::new()),
            daemon_audio: std::sync::atomic::AtomicBool::new(false),
            daemon_lanes: std::sync::atomic::AtomicU8::new(1),
            daemon_media_pipes: std::sync::atomic::AtomicBool::new(false),
            video_lane_pins: Mutex::new(HashMap::new()),
            video_route_generations: Mutex::new(VideoRouteGenerations::default()),
            video_switch_guards: Mutex::new(VideoSwitchGuards::default()),
            video_lane_binds: Mutex::new(HashMap::new()),
            disabled_networks: Mutex::new(None),
            cec: crate::cec::Cec::new(crate::cec::consent_store_path()),
        })
    }

    /// Share the disabled-networks park store with this mesh (see the field
    /// doc). Called once at assembly, before `start`.
    pub fn attach_disabled_networks(&self, store: Arc<crate::networks_store::DisabledNetworks>) {
        *self.disabled_networks.lock() = Some(store);
    }

    /// Whether `key` (config id or network id) sits parked in the shared
    /// disabled-networks store. Without a store attached nothing is parked.
    fn network_parked(&self, key: &str) -> bool {
        self.disabled_networks
            .lock()
            .as_ref()
            .is_some_and(|s| s.contains(key))
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
                            // Same lane discipline as video: drop rather than
                            // ship on lane 0 when the route has no current lane
                            // (torn down, or past the audio lane pool), which
                            // would otherwise play one stream's audio on
                            // another's route.
                            match mesh.audio_lane(&route, &peer, true) {
                                Some(lane) => {
                                    let r = mesh.send_audio_track(&peer, lane, data).await;
                                    (peer, r)
                                }
                                None => {
                                    if mesh.diag_ok(&format!("nolane-a:{route}")) {
                                        tracing::debug!(
                                            "no audio lane for {route} right now; dropping Opus frame"
                                        );
                                    }
                                    (peer, Ok(()))
                                }
                            }
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
                while let Some((peer, route_id, generation, packet, epoch, recovery)) =
                    video_rx.recv().await
                {
                    let outcome = mesh
                        .forward_video_packet(
                            &peer, &route_id, generation, packet, epoch, &recovery,
                        )
                        .await;
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
    /// Deliver one packet through the same established media functions used by
    /// the shipped shared worker. Labs scheduling changes only which bounded
    /// queue owns the packet; it does not introduce a channel, request, or
    /// signaling operation.
    async fn forward_video_packet(
        &self,
        peer: &str,
        route_id: &str,
        generation: u64,
        packet: VideoPacket,
        packet_epoch: u64,
        recovery: &VideoRecovery,
    ) -> Result<(), String> {
        if !self.video_generation_is_current(route_id, generation) {
            tracing::debug!(
                "discarding stale video AU for {route_id} generation {generation} before media send"
            );
            return Ok(());
        }
        match packet {
            VideoPacket::Jpeg(frame) => {
                for chunk in frame.into_chunks(MAX_JPEG_CHUNK_BYTES) {
                    // Teardown/re-offer can run while a large frame is being
                    // chunked. Stop at the first generation change so the
                    // predecessor cannot finish onto the successor's reused
                    // fixed track.
                    if !self.video_generation_is_current(route_id, generation) {
                        tracing::debug!(
                            "stopping stale JPEG AU for {route_id} generation {generation} during media send"
                        );
                        return Ok(());
                    }
                    let Ok(payload) = serde_json::to_value(&chunk) else {
                        continue;
                    };
                    self.send_media_value(peer, payload).await?;
                }
                Ok(())
            }
            VideoPacket::H264 {
                data,
                key,
                duration_us,
            } => {
                // Capture cannot retract deltas already in the queue when a
                // newer packet is dropped. Re-check at dequeue so none cross
                // the missing reference before the delivered repair key.
                if recovery.suppresses(Some(key)) {
                    recovery.note_suppressed(self);
                    return Ok(());
                }
                let Some(lane) = self.video_lane(route_id, peer, true) else {
                    recovery.note_drop(self, Some(key), "no route lane");
                    return Ok(());
                };
                let pace = self.video.route_pace(route_id);
                match self
                    .send_video_paced(peer, route_id, generation, lane, &data, duration_us, pace)
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => return Ok(()),
                    Err(e) => {
                        recovery.note_drop(self, Some(key), "media send failed");
                        return Err(e);
                    }
                }
                if key {
                    recovery.note_key_delivered(packet_epoch);
                }
                Ok(())
            }
        }
    }

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

    /// Send one H.264 access unit, paced when the dial is on: the unit is
    /// split at slice-NAL boundaries into ≤[`video::PACE_SLICE_BYTES`]
    /// chunks and each chunk goes out as its own track send, spaced by a
    /// small gap — on the wire, one keyframe's back-to-back packet wall
    /// becomes a few ~20-packet bursts a shallow bottleneck queue can
    /// absorb. Non-final chunks carry `duration_us = 0`, so every chunk
    /// shares one RTP timestamp. These samples are fragments of one access
    /// unit, not independent pictures; the feature is opt-in until every
    /// receive path has complete-AU finality/reassembly. This task is the route's only video
    /// sender, so chunk order is inherent; the gaps also release the
    /// pipe's writer between chunks, letting audio frames interleave
    /// instead of queueing behind a keyframe. A mid-unit send failure
    /// surfaces like any send failure; receiver recovery must discard the
    /// incomplete access unit and request a clean entry.
    ///
    /// The drain model is link-fitted: on a LAN the historical 800 Mbps
    /// shape (shallow-buffer smoothing, budget 6/10 ms) stands; on a
    /// WAN-class path the spread rate is the route's OWN send bitrate
    /// ×1.5 with a one-frame-interval budget — spreading a wall across
    /// its own frame slot adds zero pipeline latency by definition, while
    /// the old constants handed a 40 Mbps path a ~2-frame standing queue
    /// per keyframe (a ~190 KB wall in 1.7 ms is an instantaneous
    /// ~890 Mbps). `ALLMYSTUFF_PACE_DRAIN_MBPS` pins the drain for A/B.
    /// Gaps are executed against a deadline with [`os_perf::precise_sleep`]
    /// under the hood — the requested spacing is real now, not
    /// timer-wheel-rounded — and every gap is ledgered (the `pace gaps`
    /// line, one per minute).
    #[allow(clippy::too_many_arguments)]
    async fn send_video_paced(
        &self,
        peer: &str,
        route_id: &str,
        generation: u64,
        lane: u8,
        data: &[u8],
        duration_us: u64,
        pace: (bool, bool, u32, u32),
    ) -> Result<bool, String> {
        let current = || self.video_generation_is_current(route_id, generation);
        if !crate::video::paced_slices_enabled() {
            if !current() {
                return Ok(false);
            }
            self.send_video_track(peer, lane, data, duration_us).await?;
            return Ok(true);
        }
        let chunks = crate::video::split_annexb_paced(data, crate::video::PACE_SLICE_BYTES);
        if chunks.len() < 2 {
            if !current() {
                return Ok(false);
            }
            self.send_video_track(peer, lane, data, duration_us).await?;
            return Ok(true);
        }
        // (game posture, WAN-class path, current send rate bps) — the
        // shape `VideoBridge::route_pace` hands the forwarder.
        let (game, wan, rate_bps, fps) = pace;
        static DRAIN_OVERRIDE_MBPS: std::sync::LazyLock<u64> = std::sync::LazyLock::new(|| {
            std::env::var("ALLMYSTUFF_PACE_DRAIN_MBPS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0)
        });
        // (drain bps, per-gap cap µs, whole-AU budget ms).
        let (drain_bps, gap_cap_us, budget_ms) = if *DRAIN_OVERRIDE_MBPS > 0 {
            (*DRAIN_OVERRIDE_MBPS * 1_000_000, 8_000, 16)
        } else if wan && rate_bps > 0 {
            // The link is asked to carry `rate_bps` steady-state; walls
            // spread at 1.5× that (peaks are real) across at most one
            // frame interval.
            ((rate_bps as u64) * 3 / 2, 8_000, 16)
        } else if game {
            (800_000_000, 1_000, 6)
        } else {
            (800_000_000, 1_500, 10)
        };
        let total_budget = pace_budget(budget_ms, fps);
        let budget_each = total_budget / (chunks.len() as u32 - 1);
        let pace_deadline = Instant::now() + total_budget;
        let last = chunks.len() - 1;
        let mut ledger: Vec<(u64, u64)> = Vec::with_capacity(last);
        // M1's pace+write split: gap time is the ledger above; this is
        // the daemon-pipe await itself — if the daemon ever backpressures
        // (wedged reader, saturated socket), it shows here first.
        let (mut write_us, mut writes) = (0u64, 0u64);
        for (i, range) in chunks.into_iter().enumerate() {
            if !current() {
                tracing::debug!(
                    "stopping stale H.264 AU for {route_id} generation {generation} during paced media send"
                );
                return Ok(false);
            }
            let dur = if i == last { duration_us } else { 0 };
            let sent_bytes = range.len();
            let tw = Instant::now();
            self.send_video_track(peer, lane, &data[range], dur).await?;
            write_us += tw.elapsed().as_micros() as u64;
            writes += 1;
            if i != last {
                let drain_us =
                    (sent_bytes as u64 * 8_000_000 / drain_bps.max(1)).clamp(100, gap_cap_us);
                let requested = std::time::Duration::from_micros(drain_us).min(budget_each);
                // The shared media writer is the final serialization point.
                // Time waiting for it consumes this AU's pacing slot; never
                // sleep the full nominal gap after that budget has elapsed.
                let gap = pace_gap_until(pace_deadline, Instant::now(), requested);
                if !gap.is_zero() {
                    let t0 = std::time::Instant::now();
                    paced_gap(gap).await;
                    ledger.push((gap.as_micros() as u64, t0.elapsed().as_micros() as u64));
                }
            }
        }
        self.note_pace_gaps(&ledger, write_us, writes);
        Ok(true)
    }

    /// Fold one delivered video sample into the route's arrival state:
    /// time the chunk train it belongs to (same RTP timestamp), and when
    /// a new train opens, finalize the previous one into the bandwidth
    /// estimate, the delay-trend window, and the minute log (M3 + T1.1).
    fn note_video_arrival(&self, route_id: &str, rtp_timestamp: u32, bytes: usize) {
        let now = Instant::now();
        let mut map = self.video_arrivals.lock();
        let st = map
            .entry(route_id.to_string())
            .or_insert_with(|| ArrivalState {
                ts: rtp_timestamp,
                first: now,
                last: now,
                bytes: 0,
                chunks: 0,
                est_kbps: 0.0,
                window: Vec::new(),
                owd: std::collections::VecDeque::new(),
                base: None,
                last_log: now,
            });
        if st.ts != rtp_timestamp && st.chunks > 0 {
            // Train complete. Dispersion needs ≥3 timed chunks and a
            // non-degenerate spread to say anything about rate.
            let spread_us = st.last.duration_since(st.first).as_micros() as u64;
            if st.chunks >= 3 && spread_us >= 300 {
                let mbps = (st.bytes as f64 * 8.0) / spread_us as f64;
                st.window.push(mbps);
                let kbps = mbps * 1000.0;
                st.est_kbps = if st.est_kbps <= 0.0 {
                    kbps
                } else {
                    st.est_kbps * 0.8 + kbps * 0.2
                };
            }
            // One-way-delay trend: relative delay of this train's FIRST
            // chunk vs the anchor, windowed to ~2 s.
            let (base_wall, base_rtp) = *st.base.get_or_insert((st.first, st.ts));
            let rtp_delta_us = i64::from(st.ts.wrapping_sub(base_rtp) as i32) * 1000 / 90;
            let wall_delta_us = st.first.duration_since(base_wall).as_micros() as i64;
            st.owd.push_back((st.first, wall_delta_us - rtp_delta_us));
            while st
                .owd
                .front()
                .is_some_and(|(t, _)| now.duration_since(*t) > Duration::from_secs(2))
            {
                st.owd.pop_front();
            }
            // Re-anchor every ~5 min: RTP u32 wraps at ~13 h, and the
            // relative math must never straddle it.
            if st.first.duration_since(base_wall) > Duration::from_secs(300) {
                st.base = Some((st.first, st.ts));
                st.owd.clear();
            }
            if st.last_log.elapsed() >= Duration::from_secs(60) && !st.window.is_empty() {
                st.window.sort_by(f64::total_cmp);
                let p = |q: f64| st.window[((st.window.len() - 1) as f64 * q) as usize];
                tracing::info!(
                    "video in {route_id}: chunk-trains {} · implied p5 {:.1} · p50 {:.1} Mbps · est {:.1} Mbps · delay trend {:+} µs/s",
                    st.window.len(),
                    p(0.05),
                    p(0.50),
                    st.est_kbps / 1000.0,
                    Self::owd_trend_us_per_s(&st.owd),
                );
                st.window.clear();
                st.last_log = now;
            }
            (st.ts, st.first, st.bytes, st.chunks) = (rtp_timestamp, now, 0, 0);
        } else if st.chunks == 0 {
            (st.ts, st.first) = (rtp_timestamp, now);
        }
        st.last = now;
        st.bytes += bytes;
        st.chunks += 1;
    }

    /// The delay-trend slope over the window: µs of added one-way delay
    /// per second, endpoint-to-endpoint. Coarse on purpose — the signal
    /// that matters is "tens of milliseconds per second", not noise.
    fn owd_trend_us_per_s(owd: &std::collections::VecDeque<(Instant, i64)>) -> i32 {
        let (Some((t0, d0)), Some((t1, d1))) = (owd.front(), owd.back()) else {
            return 0;
        };
        let span = t1.duration_since(*t0).as_secs_f64();
        if span < 0.5 {
            return 0;
        }
        (((d1 - d0) as f64) / span) as i32
    }

    /// The estimator's current answer for a route: `(est_kbps, trend)`,
    /// zeros when unknown — what [`Self::send_video_feedback`] attaches.
    fn route_link_estimate(&self, route_id: &str) -> (u32, i32) {
        let map = self.video_arrivals.lock();
        let Some(st) = map.get(route_id) else {
            return (0, 0);
        };
        (st.est_kbps as u32, Self::owd_trend_us_per_s(&st.owd))
    }

    /// Fold one AU's gap + daemon-write measurements into the minute
    /// ledger and emit the `pace gaps` line when it's due — M2's honesty
    /// check on the pacer plus M1's pace/write split.
    fn note_pace_gaps(&self, ledger: &[(u64, u64)], write_us: u64, writes: u64) {
        if ledger.is_empty() && writes == 0 {
            return;
        }
        let mut g = self.pace_gaps.lock();
        for &(req, act) in ledger {
            g.n += 1;
            g.req_us += req;
            g.act_us += act;
            let over = act.saturating_sub(req);
            g.worst_over_us = g.worst_over_us.max(over);
            if over > 1_000 {
                g.over_1ms += 1;
            }
        }
        g.write_us += write_us;
        g.writes += writes;
        let due = g
            .last_log
            .map(|t| t.elapsed() >= std::time::Duration::from_secs(60))
            .unwrap_or(true);
        if due && g.n > 0 {
            tracing::info!(
                "pace gaps: {} gaps · requested avg {} µs → actual avg {} µs · worst +{:.1} ms · >1 ms err {:.2}% · daemon write avg {} µs/chunk",
                g.n,
                g.req_us / g.n,
                g.act_us / g.n,
                g.worst_over_us as f64 / 1000.0,
                g.over_1ms as f64 * 100.0 / g.n as f64,
                g.write_us / g.writes.max(1),
            );
            *g = PaceGapStats {
                last_log: Some(std::time::Instant::now()),
                ..PaceGapStats::default()
            };
        }
    }

    /// Send one H.264 access unit to `peer` over the daemon's video track
    /// lane — raw binary on the control socket (no base64), RTP on the wire.
    async fn send_video_track(
        &self,
        peer: &str,
        lane: u8,
        data: &[u8],
        duration_us: u64,
    ) -> Result<(), String> {
        let Some(network) = self.network_for_peer(peer) else {
            return Err("no shared network".into());
        };
        // Binary media pipe when the daemon speaks it; otherwise the legacy
        // base64 video_send op (so an older daemon still streams).
        if self.daemon_media_pipes.load(Ordering::SeqCst) {
            self.media_track_pipe
                .send_video(&network, pubkey_part(peer), lane, duration_us, data)
                .await
                .map_err(|e| e.to_string())
        } else {
            use base64::Engine as _;
            self.media_pipe
                .send(&Request::VideoSend {
                    network,
                    peer: pubkey_part(peer).to_string(),
                    stream: lane,
                    duration_us,
                    data: base64::engine::general_purpose::STANDARD.encode(data),
                })
                .await
                .map_err(|e| e.to_string())
        }
    }

    /// Send one encoded Opus frame to `peer` over the daemon's audio track
    /// lane — binary media pipe when supported, else legacy base64.
    async fn send_audio_track(&self, peer: &str, lane: u8, data: Vec<u8>) -> Result<(), String> {
        let Some(network) = self.network_for_peer(peer) else {
            return Err("no shared network".into());
        };
        if self.daemon_media_pipes.load(Ordering::SeqCst) {
            self.media_track_pipe
                .send_audio(
                    &network,
                    pubkey_part(peer),
                    lane,
                    crate::audio::OPUS_FRAME_US,
                    &data,
                )
                .await
                .map_err(|e| e.to_string())
        } else {
            use base64::Engine as _;
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
    }

    /// The network to reach `peer` on: the one we last saw them on (an inbound
    /// app frame, or the daemon's peer list — see [`Mesh::refresh_peer_networks`]),
    /// falling back to the primary. This is what lets a connection cross to a
    /// peer that only shares a secondary network with us.
    fn network_for_peer(&self, peer: &str) -> Option<String> {
        let st = self.state.lock();
        st.peer_networks
            .get(pubkey_part(peer))
            .cloned()
            .or_else(|| st.network.clone())
    }

    /// Seed `peer_networks` from the daemon's per-network peer list — the same
    /// reliable view the graph reads a peer's "online + on AllMyStuff" from.
    ///
    /// [`Mesh::network_for_peer`] otherwise learns a peer's network *only* from an
    /// inbound app frame (its presence advert, a route `Accept`, …). A peer the
    /// daemon already reports connected — so it shows online and, via its
    /// advertised endpoints, fully wireable — but that we have not yet heard from
    /// directly has no entry, so `network_for_peer` falls back to the **primary**
    /// network. A peer that shares only a **secondary** mesh then has every
    /// control/media frame addressed to the wrong network, where the daemon
    /// silently drops it: the machine "shows up online, in the graph, but the
    /// console wires up with no audio or video, and nothing else reaches it
    /// either." Learning the network from the peer list closes that gap — the
    /// first offer/update already lands on the right mesh, and the peer's reply
    /// keeps the mapping fresh thereafter.
    ///
    /// Records only a network the daemon reports the peer **reachable** on, and
    /// never clobbers one already learned from an inbound frame (that one is
    /// proven to carry traffic to us) — it just fills the gap. The stored id is
    /// the network's `config_id`, matching what an inbound frame records and what
    /// [`Mesh::prune_unjoined_peers`] reconciles against.
    async fn refresh_peer_networks(self: &Arc<Self>) {
        let networks = { self.state.lock().networks.clone() };
        for network in networks {
            let Ok(resp) = self
                .client
                .request(&Request::PeersList {
                    network: network.clone(),
                })
                .await
            else {
                continue;
            };
            let Some(peers) = resp
                .data
                .as_ref()
                .and_then(|d| d.get("peers"))
                .and_then(|v| v.as_array())
            else {
                continue;
            };
            let changed = {
                let mut st = self.state.lock();
                seed_peer_networks(&mut st.peer_networks, peers, &network);
                seed_peer_links(&mut st.peer_links, peers)
            };
            // A peer's link class landing (or flipping — an ICE-restart
            // handoff can move a link LAN→STUN mid-life) re-gates its live
            // streams' automatic dials. retune_link is a no-op unless the
            // class genuinely changes what the stream would do, so a
            // steady-state refresh costs nothing.
            for (peer, class) in changed {
                for route_id in self.video.route_ids() {
                    let owns = self
                        .route_peer(&route_id)
                        .is_some_and(|p| pubkey_part(&p) == peer);
                    if owns && self.video.retune_link(&route_id, class) {
                        tracing::info!(
                            "link to {} classified {:?} — re-gating {route_id}'s automatic video dials",
                            short_id(&peer),
                            class,
                        );
                    }
                }
            }
        }
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

    /// Bring the session online and keep it online: identify, pick a
    /// network, subscribe, pump events — and when the daemon link drops
    /// (daemon crashed, restarted, or wasn't up yet), reconnect on a capped
    /// backoff and re-run the whole bring-up. Historically this was
    /// fire-once: a failed first subscribe returned permanently and a dying
    /// event stream just emitted "disconnected", leaving a running app
    /// meshless until a full relaunch — despite two comments elsewhere
    /// promising "the event pump will retry". Now it actually does.
    pub async fn start(self: Arc<Self>) {
        // Register the runtime we're on so the engine can spawn from any
        // thread — capture/audio callbacks run on their own OS threads, where
        // a bare `tokio::spawn` panics ("no reactor running"). All engine
        // spawns go through `crate::spawn`, which uses this handle. Set first,
        // before anything (the forwarders below) spawns.
        crate::set_runtime(tokio::runtime::Handle::current());

        // A hard kill can strand Windows' `net use` entry after its loopback
        // WebDAV server is gone. Only mappings carrying our private lease
        // marker are touched; ordinary user/network drives are never swept.
        self.drive_mounts.cleanup_stale().await;

        // Spawn the media forwarders now that we're on a runtime (see
        // `spawn_media_forwarders` — `new` runs in the GUI's sync setup).
        self.spawn_media_forwarders();

        // Devices change under a running app; the watcher re-scans on a slow
        // cadence and re-advertises when the picture changed. Once for the
        // engine's life — it survives daemon-link drops untouched.
        self.spawn_inventory_watch();

        // Offers need a deadline: a route offered to a machine whose
        // AllMyStuff app died (daemon still up, so it looks present) used to
        // sit "awaiting accept" forever — a black console with no error.
        self.spawn_offer_reaper();

        // Enforce CEC consent by teardown on a ~2s sweep rather than on every
        // input frame: a lapsed grant (revoke/expiry) tears the session's
        // routes down here. Engine-lifetime; a no-op on a technician node.
        self.spawn_cec_consent_sweep();

        // The daemon-link loop: subscribe → bring up → drain events → and on
        // any end of the stream, around again with a fresh subscription and
        // a full re-bring-up (fresh client_id, channel subscribes, media
        // pipes, presence) — the daemon that comes back knows nothing about
        // the old session. Backoff 1s → 8s while the socket stays dead, reset
        // the moment a subscribe lands.
        let mesh = self.clone();
        crate::spawn(async move {
            let mut backoff = std::time::Duration::from_secs(1);
            loop {
                let (tx, mut rx) = mpsc::channel::<Value>(512);
                let client_id = match mesh.client.subscribe_events(tx).await {
                    Ok(id) => {
                        backoff = std::time::Duration::from_secs(1);
                        id
                    }
                    Err(e) => {
                        tracing::warn!(
                            "mesh: event subscribe failed ({e}); retrying in {backoff:?}"
                        );
                        mesh.emit_status("disconnected", Some(&e.to_string()));
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(std::time::Duration::from_secs(8));
                        continue;
                    }
                };
                mesh.bring_up(client_id).await;
                while let Some(value) = rx.recv().await {
                    mesh.handle_value(value).await;
                }
                // Stream ended: the daemon died or dropped the socket. Say
                // so, then go re-subscribe — this loop *is* the retry.
                tracing::warn!("mesh: daemon event stream ended — reconnecting");
                mesh.emit_status("disconnected", None);
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        });
    }

    /// One full session bring-up against a freshly-subscribed daemon link:
    /// identity → profile → networks → media-pipe probe → channel
    /// subscribes → ownership/presence. Runs on every (re)connect — after a
    /// daemon restart nothing of the old session survives daemon-side, so
    /// everything is re-established, and peers re-learn us from the fresh
    /// presence broadcast.
    async fn bring_up(self: &Arc<Self>, client_id: ClientId) {
        // Identity → our node id + presence profile. The label is the
        // user's optional override; `build_profile` falls back to the
        // hostname when it's unset.
        let me = self
            .fetch_identity()
            .await
            .unwrap_or_else(|| NodeId::this().to_string());
        let label = self.fetch_identity_label().await;
        let profile = self.build_profile(&me, label);
        // Join the claim-rendezvous networks *before* listing networks, so
        // the LAN claim network (and the claim-code network, when public
        // claims are on) is in the set we subscribe below. This is what
        // makes a fresh, otherwise-unconfigured box discoverable by a
        // same-LAN claimer with zero setup.
        self.ensure_claim_networks().await;
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

        // Probe the daemon's binary-media-pipe capability up front (the version
        // pin can't gate it — the feature predates a release). This gates the
        // inbound source pipe below and the outbound sends in
        // `send_video_track`/`send_audio_track`. A daemon without it (an older
        // build still on the socket) keeps streaming over the base64 path.
        let media_pipes = self
            .client
            .request(&Request::Status)
            .await
            .ok()
            .and_then(|r| r.data)
            .and_then(|d| d.get("media_pipes").and_then(|v| v.as_bool()))
            .unwrap_or(false);
        self.daemon_media_pipes.store(media_pipes, Ordering::SeqCst);

        // Inbound media (H.264/Opus from peers) rides a dedicated binary pipe —
        // no base64 — instead of the JSON event socket. Open it for our event
        // `client_id` before subscribing video/audio, so the daemon has the
        // sink registered when its pumps start. When the daemon doesn't speak it,
        // skip the pipe entirely — its pumps then emit base64
        // `video_inbound`/`audio_inbound` events, which the value dispatcher
        // below still decodes and handles.
        if media_pipes {
            let (media_tx, mut media_rx) = mpsc::channel::<InboundFrame>(256);
            match self
                .client
                .subscribe_media_source(client_id, media_tx)
                .await
            {
                Ok(()) => {
                    tracing::info!(
                        "binary media pipes active — H.264/Opus carry raw over the IPC (no base64) in both directions"
                    );
                    let mesh = self.clone();
                    crate::spawn(async move {
                        while let Some(f) = media_rx.recv().await {
                            match f.kind {
                                MEDIA_KIND_VIDEO => mesh.handle_video_inbound(
                                    &f.from,
                                    f.stream,
                                    f.rtp_timestamp,
                                    f.key,
                                    f.data,
                                ),
                                MEDIA_KIND_AUDIO => {
                                    mesh.handle_audio_inbound(&f.from, f.stream, f.data)
                                }
                                _ => {}
                            }
                        }
                    });
                }
                Err(e) => {
                    // Registered nothing daemon-side, so its pumps stay on base64
                    // events — still handled below.
                    tracing::warn!("mesh: media-source pipe unavailable, using base64 events: {e}");
                    self.daemon_media_pipes.store(false, Ordering::SeqCst);
                }
            }
        } else {
            tracing::info!(
                "daemon has no binary media pipes — inbound video/audio arrive as base64 events (rebuild myownmesh from this branch to enable the binary pipes)"
            );
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
            // Learn which network each *already-connected* peer lives on from the
            // daemon's peer list (their "approved" events fired before we
            // subscribed, so we'd otherwise only learn it once they send us a
            // frame). Without this the first offer/update to a peer that shares
            // only a secondary mesh is addressed to the primary and dropped.
            self.refresh_peer_networks().await;
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
        // (The inventory watcher lives in `start` — engine-lifetime, not
        // per-connect.)

        // The standing support area is a public directory, never a data mesh.
        // Enforce that at every node startup, even when no CEC GUI command has
        // run yet: persisted old pins otherwise reconnect before the app opens.
        if networks
            .iter()
            .any(|network| network == allmystuff_cec_protocol::HELP_NETWORK_ID)
        {
            self.cec_prepare_directory_area().await;
            self.cec_restore_dialed_session_rooms().await;
        }

        // CEC hygiene: the daemon persists CEC rooms and auto-rejoins them,
        // so a crash mid-ask (or an exit with the queue view armed) leaves
        // this device resident in the asking room — reading as a raised
        // hand — with nobody actually asking. A fresh node process starts
        // with both flags down, so this leaves the room.
        self.cec_sweep_stale_asking_room().await;
    }

    /// Sweep outbound route offers nobody has answered and expire them to
    /// `Rejected` with a reason the UI can show. The wire has no offer
    /// deadline and the session is deliberately clock-free, so the timer
    /// lives here: the first sweep that sees an offer stamps it, and one
    /// still `Offered` [`OFFER_TIMEOUT`] later flips to rejected — the
    /// console then explains "no answer" instead of connecting forever. A
    /// late `Accept` after expiry is harmless (the route reads rejected
    /// here; re-connecting mints a fresh route id).
    fn spawn_offer_reaper(self: &Arc<Self>) {
        const SWEEP: std::time::Duration = std::time::Duration::from_secs(5);
        const OFFER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
        let mesh = Arc::downgrade(self);
        crate::spawn(async move {
            loop {
                tokio::time::sleep(SWEEP).await;
                let Some(mesh) = mesh.upgrade() else { break };
                let mut expired: Vec<String> = Vec::new();
                {
                    let mut seen = mesh.offer_first_seen.lock();
                    let mut st = mesh.state.lock();
                    let Some(session) = st.session.as_mut() else {
                        seen.clear();
                        continue;
                    };
                    let offered: Vec<String> = session
                        .routes()
                        .filter(|r| {
                            r.origin == allmystuff_session::Origin::Outbound
                                && r.state == allmystuff_session::RouteState::Offered
                        })
                        .map(|r| r.route.id.clone())
                        .collect();
                    // Anything no longer an unanswered outbound offer stops
                    // being timed (accepted, rejected, torn down, gone).
                    seen.retain(|id, _| offered.contains(id));
                    let now = std::time::Instant::now();
                    for id in offered {
                        let first = *seen.entry(id.clone()).or_insert(now);
                        if now.duration_since(first) >= OFFER_TIMEOUT
                            && session.expire_offer(
                                &id,
                                "no answer from the far side — its AllMyStuff app may not be \
                                 running (its mesh daemon can still advertise it)",
                            )
                        {
                            seen.remove(&id);
                            expired.push(id);
                        }
                    }
                }
                if !expired.is_empty() {
                    for id in &expired {
                        tracing::warn!(
                            "route offer {id} went unanswered for {OFFER_TIMEOUT:?} — expired \
                             (is the far side's AllMyStuff app running?)"
                        );
                    }
                    mesh.emit_snapshot();
                }
            }
        });
    }

    /// Enforce CEC consent by teardown on a slow sweep instead of on every
    /// frame. A dialed technician's screen-view and control authority is the
    /// customer's live consent grant; that grant is checked once when a route
    /// is *offered* (admission — the offer gate's [`Self::sender_may_drive`] /
    /// [`Self::cec_screen_offer_denied`]) and then **not re-evaluated per
    /// frame**. This sweep is the other half: every [`SWEEP`] it re-checks each
    /// live CEC route against those same gates and tears down any that a lapsed
    /// grant — revoked, expired, or an "Approve Once" that ended — no longer
    /// covers. A revoke that lands between sweeps still bites at once through its
    /// own explicit teardown ([`Self::cec_revoke`]); this backstops **expiry**,
    /// which nothing else tears down, and closes its screen twin — a lapsed
    /// grant used to leave the customer *still streaming their screen* until they
    /// disconnected, because only the input plane was gated per frame. The cost
    /// is one grant evaluation per live CEC route every couple of seconds, versus
    /// the tens per second an input stream drove. Customer-side only: a
    /// technician node hosts nothing consent-gated and `knows_technician` is
    /// false there, so the body no-ops.
    fn spawn_cec_consent_sweep(self: &Arc<Self>) {
        const SWEEP: std::time::Duration = std::time::Duration::from_secs(2);
        let mesh = Arc::downgrade(self);
        crate::spawn(async move {
            // The last `cec://viewing` map this sweep emitted — technician
            // canonical id → (screen live, control live). `None` until the
            // first pass so a fresh node always emits once (even an empty
            // map), giving a GUI that hydrated before us a baseline.
            let mut last_viewing: Option<std::collections::BTreeMap<String, (bool, bool)>> = None;
            loop {
                tokio::time::sleep(SWEEP).await;
                let Some(mesh) = mesh.upgrade() else { break };
                // NOTE: no `is_technician` skip here. That early-out assumed a
                // technician node hosts nothing consent-gated — but the role
                // flips permanently on the first dial, and a DUAL-ROLE node
                // (dialed someone once, yet also reachable as a customer) very
                // much still hosts a consent-gated screen. On a pure technician
                // node the body no-ops anyway: its routes point at customers,
                // and `knows_technician` is false for those peers.
                // Snapshot every live route (peer, id, is-screen, drive-plane)
                // under the state lock, then drop it before touching the CEC
                // store or tearing anything down (`disconnect` re-locks state).
                let routes: Vec<(String, Route, Option<DrivePlane>)> = {
                    let st = mesh.state.lock();
                    match st.session.as_ref() {
                        Some(session) => session
                            .routes()
                            .map(|r| {
                                (
                                    r.peer.as_str().to_string(),
                                    r.route.clone(),
                                    route_drive_plane(&r.route),
                                )
                            })
                            .collect(),
                        None => Vec::new(),
                    }
                };
                let mut stale_routes: Vec<String> = Vec::new();
                let mut lapsed_techs: std::collections::BTreeSet<String> =
                    std::collections::BTreeSet::new();
                // What each technician is ACTUALLY doing right now, from the
                // routes themselves: screen = a live display route, control = a
                // live input route. This — not session state — is what the
                // customer's "Viewing/Controlling your screen" chip keys on:
                // a technician closing their console tears the routes down but
                // sends no session event, and chat keeps the session itself
                // alive, so session state over-claims. A route this very pass
                // is tearing down for lapsed consent doesn't count.
                let mut viewing: std::collections::BTreeMap<String, (bool, bool)> =
                    std::collections::BTreeMap::new();
                for (peer, route, plane) in routes {
                    // Only CEC technicians are consent-gated; an owner/fleet or
                    // ordinary peer's routes are none of this sweep's business.
                    if !mesh.cec.knows_technician(&peer) {
                        continue;
                    }
                    let is_screen = matches!(route.media, MediaKind::Display | MediaKind::Video);
                    let screen_lapsed = is_screen
                        && cec_screen_consent_blocks(
                            mesh.sender_may_source_media(&peer, &route),
                            mesh.cec_screen_offer_denied(&peer),
                        );
                    let drive_lapsed = plane.is_some_and(|pl| !mesh.sender_may_drive(&peer, pl));
                    if screen_lapsed || drive_lapsed {
                        stale_routes.push(route.id.clone());
                        lapsed_techs.insert(crate::cec::pubkey_part(&peer).to_string());
                        continue;
                    }
                    let entry = viewing
                        .entry(crate::cec::pubkey_part(&peer).to_string())
                        .or_insert((false, false));
                    entry.0 |= is_screen;
                    entry.1 |= plane == Some(DrivePlane::Input);
                }
                if !stale_routes.is_empty() {
                    for id in &stale_routes {
                        tracing::info!(
                            "CEC consent lapsed — tearing down route {id} (approval revoked or expired)"
                        );
                        let _ = mesh.disconnect(id.clone()).await;
                    }
                    // End the lapsed technicians' sessions so the customer's
                    // "connected" banner clears — a route teardown alone emits no
                    // session event — and retire any leftover "Approve Once" grant.
                    for tech in lapsed_techs {
                        for sid in mesh.cec.end_sessions_for(&tech) {
                            mesh.sink.emit(
                                "cec://session",
                                json!({ "session_id": sid, "state": "ended" }),
                            );
                        }
                        mesh.cec.retire_once(&tech);
                    }
                    mesh.cec_emit_grants();
                    mesh.emit_snapshot();
                }
                // Tell the GUI what's live whenever the picture changes (and
                // once at startup, so a GUI that hydrated before this sweep
                // gets its baseline). The event carries the whole map, so a
                // missed frame self-heals on the next change.
                if last_viewing.as_ref() != Some(&viewing) {
                    mesh.sink.emit("cec://viewing", cec_viewing_value(&viewing));
                    last_viewing = Some(viewing.clone());
                }
                // An approved support session includes the customer's attached
                // KVM from the start. Both halves stay short leases: discovery
                // on the technician and authority on the KVM disappear shortly
                // after the support session ends.
                mesh.refresh_kvm_support().await;
                for kvm in mesh.cec.prune_support_kvms() {
                    mesh.drop_transient_support_kvm(&kvm).await;
                }
            }
        });
    }

    /// `cec_viewing` (customer): what each connected technician is actually
    /// doing right now — `{ techs: { <canonical tech>: { screen, control } } }`
    /// — derived from the LIVE routes, not session state. The event twin
    /// (`cec://viewing`) is pushed by the consent sweep on every change; this
    /// command is the pull for GUI hydrate, so an app that starts mid-session
    /// paints the chip without waiting for a transition.
    pub async fn cec_viewing(self: &Arc<Self>) -> Result<Value, String> {
        let mut viewing: std::collections::BTreeMap<String, (bool, bool)> =
            std::collections::BTreeMap::new();
        {
            let st = self.state.lock();
            if let Some(session) = st.session.as_ref() {
                for r in session.routes() {
                    let peer = r.peer.as_str();
                    if !self.cec.knows_technician(peer) {
                        continue;
                    }
                    let entry = viewing
                        .entry(crate::cec::pubkey_part(peer).to_string())
                        .or_insert((false, false));
                    entry.0 |= matches!(r.route.media, MediaKind::Display | MediaKind::Video);
                    entry.1 |= route_drive_plane(&r.route) == Some(DrivePlane::Input);
                }
            }
        }
        Ok(cec_viewing_value(&viewing))
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
                        Self::advertised_capabilities(&inv, &node),
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
            // Cloned (not moved) so `fleet_owner` below can fall back to this
            // device's label for an unnamed fleet it owns.
            label: label.clone(),
            hostname,
            summary: allmystuff_bridge::node_summary(&inv),
            capabilities: Self::advertised_capabilities(&inv, &node),
            // Tell peers who owns this device and whether it's up for
            // adoption, so they can't silently grab a box that's already
            // spoken for (or one that was never put into claim mode).
            owner: self.ownership.owner().map(NodeId::from),
            claimable: self.ownership.claimable(),
            boot: self.boot_id.load(Ordering::Relaxed),
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
                    allmystuff_protocol::FEATURE_FILES.to_string(),
                    allmystuff_protocol::FEATURE_ROOMS.to_string(),
                    allmystuff_protocol::FEATURE_SITES.to_string(),
                ];
                // Hosting a shell or a camera stream needs the capture
                // planes — a capture-less build (iOS) must not invite
                // offers its stubs would refuse.
                #[cfg(feature = "host")]
                {
                    f.push(allmystuff_protocol::FEATURE_TERMINAL.to_string());
                    f.push(allmystuff_protocol::FEATURE_CAMERA.to_string());
                }
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
            // The fleet's display name ("Casey"), shared fleet-wide (handed
            // down with the fleet key), so a peer groups + labels this device's
            // fleet straight from presence. Empty when not in a fleet / unnamed.
            fleet_name: self.ownership.fleet_name(),
            // The fleet **owner's** (person) name — never the owner device's
            // hostname. See [`Mesh::fleet_owner_name`].
            fleet_owner: self.fleet_owner_name(&label),
            // An ordinary machine is not a KVM appliance — only a NanoKVM-class
            // device (its Go mesh bridge) ever fills this in. See FEATURE_KVM.
            kvm: None,
            // Stamped per send (broadcast_presence / send_presence_to), not
            // at build: a profile can sit in state for minutes between
            // sends, and a stale stamp would read as clock skew.
            sent_at: 0,
        }
    }

    /// The fleet owner's display name to advertise in presence — the *person*
    /// who owns the fleet, never the owner device's hostname. A fleet is named
    /// for its owner, so this is the fleet name when one is set; otherwise the
    /// owner device falls back to its own label (`own_label`) so an as-yet-
    /// unnamed fleet still says *who* owns it, while a member of an unnamed
    /// fleet leaves it empty (it can't name the owner until the fleet is named
    /// or — once roles converge — the signed roster tells it who the owner is).
    fn fleet_owner_name(&self, own_label: &str) -> String {
        let name = self.ownership.fleet_name();
        if !name.trim().is_empty() {
            name
        } else if self.ownership.is_fleet_owner() {
            own_label.to_string()
        } else {
            String::new()
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
        let Some(mut profile) = profile else { return };
        // Stamp our wall clock at the moment of send — receivers read it as
        // a passive clock-skew sample (see NodeProfile::sent_at).
        profile.sent_at = unix_now_ms();
        for network in networks {
            // Never broadcast this node's profile on the CEC rooms. They are
            // world-joinable (any stranger can be co-resident on the support
            // area), and the profile is the graph: broadcasting it there is
            // how every customer's machine once showed up as a "computer" in
            // every other customer's app. Peers with a real CEC relationship
            // (a dialed customer, a consented technician) get the profile
            // *targeted* over the session instead — see `send_presence_to`
            // and the connection-approved re-assert.
            if crate::cec::is_cec_network(&network) {
                continue;
            }
            // Claimable presence is per-network: only the claim-rendezvous
            // networks ever carry `claimable: true` (see
            // `claimable_advertised_on`) — on every other mesh this device
            // reads as a plain, unclaimable node, so it can't be discovered
            // for claiming over the public mesh unless that's deliberately
            // enabled here.
            let mut scoped = profile.clone();
            scoped.claimable = profile.claimable && self.claimable_advertised_on(&network);
            let Ok(payload) = serde_json::to_value(&scoped) else {
                continue;
            };
            let _ = self
                .client
                .request(&Request::ChannelSendAll {
                    network,
                    channel: CHANNEL_PRESENCE.to_string(),
                    payload,
                })
                .await;
        }
    }

    /// Whether `claimable: true` may be advertised on `network`: the LAN
    /// claim network always; this device's own claim-code network while
    /// public claims are enabled; and — for a legacy claimer that only
    /// shares an ordinary mesh with us — anywhere, once public claims are
    /// deliberately on. Mirrors [`Mesh::claim_network_allowed`], so we
    /// never advertise somewhere we'd then decline.
    fn claimable_advertised_on(&self, network: &str) -> bool {
        self.claim_network_allowed(network)
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
                // Base64 fallback path (a daemon without the binary media-source
                // pipe): decode here so the handler always gets raw bytes.
                use base64::Engine as _;
                let Ok(data) = base64::engine::general_purpose::STANDARD.decode(data) else {
                    return;
                };
                self.handle_video_inbound(from, stream, rtp_timestamp, key, data);
            }
            "audio_inbound" => {
                let from = value.get("from").and_then(|v| v.as_str()).unwrap_or("");
                let Some(data) = value.get("data").and_then(|v| v.as_str()) else {
                    return;
                };
                let stream = value.get("stream").and_then(|v| v.as_u64()).unwrap_or(0) as u8;
                use base64::Engine as _;
                let Ok(data) = base64::engine::general_purpose::STANDARD.decode(data) else {
                    return;
                };
                self.handle_audio_inbound(from, stream, data);
            }
            "event" => {
                if let Some(event) = value.get("event") {
                    let peer_kind = event
                        .get("event_kind")
                        .and_then(|v| v.as_str())
                        .filter(|k| *k == "peer")
                        .and(event.get("kind").and_then(|v| v.as_str()));
                    let event_network = event
                        .get("network_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    // The asking room's signaling presence IS the help queue:
                    // a device surfacing there has its hand up; one dropping
                    // out (leave, crash timeout) has withdrawn. Only a
                    // watching technician keeps the cache; the polled
                    // `cec_help_list` reconcile heals any event this stream
                    // drops.
                    if event_network == allmystuff_cec_protocol::ASK_NETWORK_ID
                        && self.cec.watching_help()
                    {
                        if let Some(device) = event.get("device_id").and_then(|v| v.as_str()) {
                            let is_self = self
                                .local_node_id()
                                .is_some_and(|me| pubkey_part(&me) == pubkey_part(device));
                            let changed = match peer_kind {
                                Some("sighted") if !is_self => self.cec.help_present(device),
                                Some("dropped") => self.cec.remove_help_beacon(device),
                                _ => false,
                            };
                            if changed {
                                self.sink
                                    .emit("cec://help", json!({ "waiting": self.cec.help_list() }));
                            }
                        }
                    }
                    // Connection establishment is a claim-status trigger: a
                    // peer just went live for app traffic ("approved"), so
                    // re-assert presence + fleet roster straight at it —
                    // there is no heartbeat to catch it up later.
                    let approved = peer_kind == Some("approved");
                    if approved {
                        if let Some(device) = event.get("device_id").and_then(|v| v.as_str()) {
                            // On a CEC room, first contact is gated on a
                            // deliberate CEC relationship — a co-resident
                            // stranger that talked its way to mesh-ACTIVE
                            // (an old open-era build) gets no ownership
                            // probe, no presence, no roster from us. (The
                            // event still reaches the GUI's Activity log
                            // below — only the outreach is withheld.)
                            let gated = crate::cec::is_cec_network(event_network)
                                && !self.cec.relationship_with(device);
                            if !gated {
                                let mesh = self.clone();
                                let device = device.to_string();
                                crate::spawn(async move {
                                    // Record which network this peer just went
                                    // live on *before* anything is sent to it
                                    // (the ownership check below included):
                                    // otherwise the very first frame to a peer
                                    // sharing only a secondary mesh falls back
                                    // to the primary network and is dropped.
                                    mesh.refresh_peer_networks().await;
                                    mesh.ownership_check(Some(&device)).await;
                                });
                            }
                        }
                    }
                    // The daemon proved THIS DEVICE was evicted from a
                    // network by its signed governance (it verified the
                    // log itself — this is not a peer's claim). If that
                    // network is our FLEET mesh, the fleet is over for
                    // this device: run the same teardown the owner's
                    // cooperative Release performs, so an eviction that
                    // happened while we were offline finally cleans up
                    // instead of leaving a dead credential camping on a
                    // mesh that denies it everywhere. Any other network's
                    // eviction is daemon-side only (it already stood the
                    // engine down); nothing to tear here.
                    if event.get("event_kind").and_then(|v| v.as_str()) == Some("diag")
                        && event.get("category").and_then(|v| v.as_str()) == Some("governance")
                        && event
                            .get("detail")
                            .and_then(|d| d.get("hint"))
                            .and_then(|v| v.as_str())
                            == Some("self_evicted")
                    {
                        let evicted_net = event
                            .get("network_id")
                            .and_then(|v| v.as_str())
                            .map(str::to_string);
                        let fleet_net = self.ownership.fleet_network_id();
                        if evicted_net.is_some() && evicted_net == fleet_net {
                            tracing::warn!(
                                "the fleet's signed governance evicted this device — clearing fleet state"
                            );
                            let mesh = self.clone();
                            crate::spawn(async move {
                                mesh.apply_fleet_release().await;
                            });
                        }
                    }
                    // The daemon's own clock diagnostic (its heartbeat-based
                    // estimator, on daemons new enough to run one): surface
                    // it on the same UI event the presence-based estimate
                    // uses, so the front-end has one warning to render
                    // whichever detector fired first.
                    if event.get("event_kind").and_then(|v| v.as_str()) == Some("diag")
                        && event.get("category").and_then(|v| v.as_str()) == Some("clock")
                    {
                        let warn = event.get("level").and_then(|v| v.as_str()) == Some("warn");
                        let detail = event.get("detail").cloned().unwrap_or(Value::Null);
                        self.sink.emit(
                            "allmystuff://clock-skew",
                            serde_json::json!({
                                "state": if warn { "warn" } else { "clear" },
                                "skew_ms": detail.get("skew_ms").cloned().unwrap_or(Value::Null),
                                "peers": detail.get("peers").cloned().unwrap_or(Value::Null),
                                "message": event.get("message").cloned().unwrap_or(Value::Null),
                                "source": "daemon",
                            }),
                        );
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
        // Reject CEC strangers before recording *any* app-layer state about
        // them. The standing support room is world-joinable; during migration
        // an old Open roster can briefly establish a data channel before the
        // room is healed back to Silent. Recording that contact first left a
        // CEC-only network provenance behind even though the presence itself
        // was rejected, which in turn kept the technician machine eligible for
        // the AllMyStuff graph until the next full node restart.
        if channel == CHANNEL_PRESENCE
            && crate::cec::is_cec_network(&network)
            && !self.cec.relationship_with(&from)
        {
            tracing::debug!(
                "ignoring presence on {network} from {} — no CEC relationship",
                short_id(&from)
            );
            return;
        }
        // A customer may temporarily introduce its attached KVM, but the
        // appliance's own authoritative binding must corroborate that exact
        // customer. This prevents a customer from naming an arbitrary support-
        // room peer and using the exception to inject it into the tech's graph.
        if channel == CHANNEL_PRESENCE
            && crate::cec::is_cec_network(&network)
            && !self.cec.direct_relationship_with(&from)
        {
            if let Some(customer) = self.cec.support_kvm_customer(&from) {
                let attached = payload
                    .get("kvm")
                    .and_then(|kvm| kvm.get("attached_to"))
                    .and_then(Value::as_str);
                if attached.is_none_or(|node| !same_node(node, &customer)) {
                    tracing::warn!(
                        "ignoring transient KVM presence from {}: attachment does not match customer {}",
                        short_id(&from),
                        short_id(&customer)
                    );
                    return;
                }
            }
        }
        // Remember which network this peer is reachable on, so control/media
        // we send back goes to the right one (a peer may share only one of the
        // several networks we're on).
        if !network.is_empty() && !from.is_empty() {
            self.state
                .lock()
                .peer_networks
                .insert(pubkey_part(&from).to_string(), network.clone());
        }
        match channel {
            CHANNEL_PRESENCE => {
                // On the CEC rooms, a profile advert is accepted only from a
                // peer this node has a deliberate CEC relationship with (a
                // customer this technician dialed; a technician this customer
                // granted or is being asked to approve). The rooms are
                // world-joinable, so without this gate any co-resident
                // stranger that connects (an old open-era build, a direct
                // dialer) would inject itself into the graph as a "computer"
                // — and, worse, the first-contact answer below would send our
                // profile AND roster straight back to it. Drop before any
                // state is touched.
                // Never silently discard a node-information update on a parse
                // slip: a peer's presence is how we learn its name, owner,
                // sites, version and fleet, so a dropped advert is a node that
                // never appears or never refreshes. Parse once and log the
                // reason on failure — failing closed with no trace is what hid
                // this for so long. The profile is lenient about absent/older
                // fields now (they default), so a hard error here is genuinely
                // malformed input worth seeing.
                let parsed = serde_json::from_value::<NodeProfile>(payload);
                if let Err(e) = &parsed {
                    tracing::warn!("dropping presence advert from {}: {e}", short_id(&from));
                }
                if let Ok(profile) = parsed {
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
                    // A stamped advert is a free clock-skew sample: the
                    // sender's wall clock at send vs ours at receipt
                    // (delivery is one data-channel hop — milliseconds,
                    // noise against the 10 s threshold). Absent (`0`) on
                    // older senders; skipped, never guessed.
                    if !is_self && profile.sent_at > 0 {
                        let sample = profile.sent_at as i64 - unix_now_ms() as i64;
                        self.note_peer_clock(&canon, sample);
                    }
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
                    // A fresh boot id from a peer we already knew: every route
                    // wired to its PREVIOUS incarnation is dead on its side —
                    // but ours would keep capturing and encoding into the void
                    // (the far end logs "no route maps to it" for as long as
                    // the orphan lives, and its stale lane pin can shadow the
                    // next session's stream). Reap them now; the fresh
                    // incarnation is folded right back in below and re-offers
                    // whatever it actually wants.
                    if new_boot && known {
                        let effects = {
                            let mut st = self.state.lock();
                            st.session
                                .as_mut()
                                .map(|s| s.reap_peer_routes(&node_id))
                                .unwrap_or_default()
                        };
                        if !effects.is_empty() {
                            tracing::info!(
                                "peer {} restarted — reaping {} stale route(s) to its previous incarnation",
                                short_id(&from),
                                effects.len()
                            );
                            self.process_effects(effects).await;
                        }
                    }
                    let changed = {
                        let mut st = self.state.lock();
                        st.session
                            .as_mut()
                            .map(|s| s.apply_presence(profile))
                            .unwrap_or(false)
                    };
                    // A mapped drive is user intent, not an ICE-session lease.
                    // If the source laptop woke into a fresh incarnation, the
                    // route reap above made its old drive route terminal; this
                    // re-requests it using the same path/name/letter. Calling
                    // on every advert is safe: active routes and in-flight
                    // reconnects are both filtered inside.
                    self.reconnect_drive_pulls(node_id.as_str());
                    // Self-heal the fleet: if a device we still list as a fleet
                    // member now advertises a *positively different* owner, it
                    // has been re-claimed — evict it so the roster reflects
                    // reality even when the explicit leave notification never
                    // arrived (it was offline, crashed, or was claimed straight
                    // out from under us).
                    //
                    // An advert with *no* owner is not departure evidence: it's
                    // ambiguous between "went unclaimed" and a merely-defaulted
                    // field (an advert sent before the peer's ownership store
                    // loaded, an older build, a foreign bridge like the KVM's).
                    // Dropping on it authors a signed Evict tombstone that the
                    // daemon's roster convergence then mirrors onto *every*
                    // fleet device — permanently stripping the member from the
                    // rosters that authorize remote control, which surfaced as
                    // "video streams but keyboard/mouse are refused". A device
                    // that truly went unclaimed keeps advertising ownerless and
                    // claimable; evict it when it positively advertises its new
                    // owner, or deliberately from the fleet UI.
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
                        if in_my_fleet
                            && fleet_departure(advertised_owner.as_deref(), me.as_deref())
                        {
                            tracing::info!(
                                "fleet member {} now answers to a different owner — dropping",
                                short_id(node_id.as_str())
                            );
                            self.fleet_drop_member(node_id.to_string()).await;
                        } else if in_my_fleet && still_ours {
                            if new_boot || !known {
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
                            // Signed-roster self-heal. The claim-time admit authors a
                            // new member's RoleGrant *before* that member has joined the
                            // fleet network — the key handoff (and so the join) lands
                            // after — and a constrained co-node like a KVM appliance can
                            // be absent from the closed net at that instant, so the grant
                            // never takes and nothing re-admits it until a restart. The
                            // member then sits on the fleet mesh yet missing from the
                            // signed roster the graph reads: it shows "unclaimed /
                            // unknown fleet" with its owner-gated controls (web Site,
                            // reboot, Wi-Fi, unclaim) dead, even though it holds the right
                            // key and name. So whenever a still-ours member is present but
                            // not yet in our signed roster, re-run the idempotent admit;
                            // the admit loop skips members already in the log, so this
                            // quiesces the moment the roster converges.
                            if !self.fleet_authorized.lock().contains(&peer) {
                                tracing::info!(
                                    "fleet member {} is present but unsigned — admitting it to the fleet roster",
                                    short_id(node_id.as_str())
                                );
                                self.ensure_fleet_network().await;
                                self.refresh_fleet_authorization().await;
                            }
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
                        // (The old "re-beacon the help room at whoever just
                        // came up" nudge lived here. A raised hand is
                        // asking-room *membership* now — the engine's own
                        // signaling announces cover every watcher, new or
                        // late, with nothing to re-send.)
                    }
                    if changed {
                        self.emit_snapshot();
                    }
                }
            }
            CHANNEL_CONTROL => {
                if let Ok(msg) = serde_json::from_value::<ControlMessage>(payload) {
                    // Claims are gated **by arrival network** before the
                    // session ever sees them (the network id is dropped at
                    // the `Effect::Ownership` boundary, so this is the only
                    // place that can enforce it): the LAN claim network is
                    // always honored, anything else only when public claims
                    // are deliberately enabled on this device. The decline
                    // names the fix so the claimer's toast is actionable.
                    if let ControlMessage::Ownership(OwnershipControl::Claim { .. }) = &msg {
                        if !self.claim_network_allowed(&network) {
                            tracing::warn!(
                                "claim from {} over {network:?} refused — claims over the \
                                 public mesh are disabled on this device",
                                short_id(&from)
                            );
                            let _ = self
                                .send_control(
                                    &from,
                                    &ControlMessage::Ownership(OwnershipControl::Declined {
                                        reason: "claims over the public mesh are disabled on \
                                                 this device — claim it from the same local \
                                                 network instead"
                                            .into(),
                                    }),
                                )
                                .await;
                            return;
                        }
                    }
                    // Terminal and files offers are screened *before* the
                    // session sees them: the session auto-accepts (Accept +
                    // StartMedia in one step), and a shell — or this disk —
                    // is owner/fleet-only, the same rule as input injection,
                    // enforced before any reply exists.
                    if let ControlMessage::Route(RouteControl::Offer { route, drive, .. }) = &msg {
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
                            .is_some_and(|me| route_sources_on(route, &me));
                        // Authorized for this exact plane: owner/fleet, or a
                        // share grant the owner extended for it. Non-privileged
                        // routes (`None` plane) are never refused here.
                        let accepted_drive_pull = if is_mapped_drive_route(route) {
                            drive
                                .as_ref()
                                .and_then(|offer| offer.request.as_ref())
                                .and_then(|request| {
                                    let mut pulls = self.drive_pull_tokens.lock();
                                    let matches = pulls.get(request).is_some_and(|pull| {
                                        same_node(&pull.source, &from)
                                            && pull.made.elapsed() < Duration::from_secs(120)
                                    });
                                    matches.then(|| pulls.remove(request)).flatten()
                                })
                        } else {
                            None
                        };
                        let explicit_drive_pull = accepted_drive_pull.is_some();
                        // Drive pushes TO this machine need the Files plane:
                        // owner/fleet, an explicit Files share, or a live CEC
                        // Control grant. A receiver-minted pull token is the
                        // opposite-direction exception: it lets this user pull
                        // FROM a Files share/support source without turning that
                        // share into unsolicited push access.
                        let authorized = if is_mapped_drive_route(route) {
                            mapped_drive_offer_authorized(
                                explicit_drive_pull,
                                self.sender_may_drive(&from, DrivePlane::Files),
                            )
                        } else {
                            route_drive_plane(route)
                                .is_none_or(|plane| self.sender_may_drive(&from, plane))
                        };
                        if authorized && is_mapped_drive_route(route) {
                            let reconnect = accepted_drive_pull
                                .map(|pull| DriveReconnect {
                                    source: pull.source,
                                    root: pull.root,
                                    label: pull.label,
                                    mount: pull.mount,
                                })
                                .or_else(|| {
                                    let offer = drive.as_ref()?;
                                    Some(DriveReconnect {
                                        source: pubkey_part(from.as_str()).to_string(),
                                        root: offer.root.clone()?,
                                        label: offer.label.clone(),
                                        mount: offer.mount.clone(),
                                    })
                                });
                            if let Some(reconnect) = reconnect {
                                self.drive_reconnects
                                    .lock()
                                    .insert(route.id.clone(), reconnect);
                                self.persist_drive_reconnects();
                            }
                        }
                        // CEC screen gate: a customer only lets a dialed
                        // technician view its screen while a live consent grant
                        // covers it — the screen twin of the per-plane
                        // `sender_may_drive` gate (an ordinary Display route is
                        // `plane: None`, so it wouldn't otherwise be screened
                        // here).
                        if hosts_here
                            && matches!(route.media, MediaKind::Display | MediaKind::Video)
                            && cec_screen_consent_blocks(
                                self.sender_may_source_media(&from, route),
                                self.cec_screen_offer_denied(&from),
                            )
                        {
                            tracing::warn!(
                                "CEC screen offer {} from {} refused: no live consent grant",
                                route.id,
                                short_id(&from)
                            );
                            let _ = self
                                .send_control(
                                    &from,
                                    &ControlMessage::Route(RouteControl::Reject {
                                        route_id: route.id.clone(),
                                        reason: "the customer hasn't approved screen sharing \
                                                 for you (or revoked it)"
                                            .into(),
                                    }),
                                )
                                .await;
                            return;
                        }
                        // Media-source gate: a Display/Video/Audio offer whose
                        // source endpoint is a capability on THIS machine makes
                        // us capture our own screen/camera/microphone and stream
                        // it to the offerer — every bit as sensitive as letting
                        // them drive us, but `route_drive_plane` never classified
                        // it, so the `authorized` computed above is
                        // unconditionally true for it. Require the same
                        // owner/fleet-or-explicit-grant authority here. (A known
                        // CEC technician without live consent was already refused
                        // just above; `sender_may_source_media` honours a
                        // technician's live ScreenView grant and person-to-person
                        // screen/camera/mic shares.)
                        if hosts_here
                            && matches!(
                                route.media,
                                MediaKind::Display | MediaKind::Video | MediaKind::Audio
                            )
                            && !self.sender_may_source_media(&from, route)
                        {
                            tracing::warn!(
                                "media source offer {} from {} refused: not owner/fleet/share",
                                route.id,
                                short_id(&from)
                            );
                            let _ = self
                                .send_control(
                                    &from,
                                    &ControlMessage::Route(RouteControl::Reject {
                                        route_id: route.id.clone(),
                                        reason:
                                            "not authorized: capturing this device's screen, \
                                                 camera, or microphone needs owner/fleet or a share"
                                                .into(),
                                    }),
                                )
                                .await;
                            return;
                        }
                        if let Some(reason) =
                            privileged_offer_refusal(route, hosts_here, authorized)
                        {
                            tracing::warn!(
                                "privileged offer {} from {} refused: not owner/fleet/share",
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
                    // Only a periodic viewer report produced after the close's
                    // minimum age proves the replacement survived. One-shot
                    // Offer/Accept/Tune/Lane controls can already be in flight
                    // and are deliberately not treated as liveness.
                    if let Some(route_id) = inbound_video_feedback_liveness_route_id(&msg) {
                        if let Some(token) = self.cancel_pending_video_teardown(route_id, &from) {
                            tracing::warn!(
                                route = %route_id,
                                from = %short_id(&from),
                                network = %network,
                                token,
                                disposition = "quarantine_canceled_by_liveness",
                                "inbound video route control"
                            );
                        }
                    }

                    // Teardown used to be the one destructive route control
                    // that was not peer-checked. Authentication, guard choice,
                    // and (when committing) Session mutation are one state-lock
                    // transaction, so a same-id replacement cannot enter in
                    // between and be killed by an old peer message.
                    if let ControlMessage::Route(RouteControl::Teardown { route_id }) = &msg {
                        let (facts, gate, effects) = {
                            let mut st = self.state.lock();
                            let Some(session) = st.session.as_mut() else {
                                return;
                            };
                            let facts = session.route(route_id).map(|r| {
                                (r.peer.as_str().to_string(), r.state.clone(), r.route.media)
                            });
                            if facts
                                .as_ref()
                                .is_some_and(|(peer, _, _)| pubkey_part(peer) != pubkey_part(&from))
                            {
                                tracing::warn!(
                                    route = %route_id,
                                    from = %short_id(&from),
                                    network = %network,
                                    expected = %facts.as_ref().map(|f| short_id(&f.0)).unwrap_or_default(),
                                    disposition = "foreign_peer_refused",
                                    "inbound route teardown"
                                );
                                return;
                            }
                            let eligible = facts.as_ref().is_some_and(|(_, state, media)| {
                                *state == RouteState::Active
                                    && matches!(media, MediaKind::Display | MediaKind::Video)
                            });
                            let gate = if eligible {
                                self.video_switch_guards.lock().gate_inbound_teardown(
                                    route_id,
                                    &from,
                                    Instant::now(),
                                )
                            } else {
                                InboundVideoTeardownGate::Commit
                            };
                            let effects = if matches!(gate, InboundVideoTeardownGate::Commit) {
                                session.handle(
                                    NodeId::from(from.as_str()),
                                    ControlMessage::Route(RouteControl::Teardown {
                                        route_id: route_id.clone(),
                                    }),
                                )
                            } else {
                                Vec::new()
                            };
                            (facts, gate, effects)
                        };
                        let generation = self.video_route_generations.lock().current(route_id);
                        let state_before = facts.as_ref().map(|(_, state, _)| state);
                        let media = facts.as_ref().map(|(_, _, media)| media);
                        match gate {
                            InboundVideoTeardownGate::Quarantine {
                                predecessor,
                                age,
                                token,
                                incarnation,
                            } => {
                                tracing::warn!(
                                    route = %route_id,
                                    from = %short_id(&from),
                                    network = %network,
                                    state_before = ?state_before,
                                    media = ?media,
                                    generation = ?generation,
                                    predecessor = %predecessor,
                                    age_us = age.as_micros(),
                                    token,
                                    incarnation,
                                    quarantine_ms = VIDEO_INBOUND_TEARDOWN_QUARANTINE.as_millis(),
                                    disposition = "quarantined",
                                    "inbound route teardown"
                                );
                                let mesh = self.clone();
                                let route_id = route_id.clone();
                                crate::spawn(async move {
                                    mesh.commit_quarantined_video_teardown(
                                        route_id,
                                        from,
                                        network,
                                        token,
                                        incarnation,
                                    )
                                    .await;
                                });
                                return;
                            }
                            InboundVideoTeardownGate::CoalesceDuplicate { token } => {
                                tracing::warn!(
                                    route = %route_id,
                                    from = %short_id(&from),
                                    network = %network,
                                    state_before = ?state_before,
                                    media = ?media,
                                    generation = ?generation,
                                    token,
                                    disposition = "duplicate_coalesced",
                                    "inbound route teardown"
                                );
                                return;
                            }
                            InboundVideoTeardownGate::Commit => {
                                tracing::info!(
                                    route = %route_id,
                                    from = %short_id(&from),
                                    network = %network,
                                    state_before = ?state_before,
                                    media = ?media,
                                    generation = ?generation,
                                    disposition = "commit",
                                    "inbound route teardown"
                                );
                                if matches!(media, Some(MediaKind::Storage)) {
                                    // A teardown deliberately sent by the
                                    // source is a real unmap. Peer-restart
                                    // reaps never arrive through this arm, so
                                    // those retain their reconnect intent.
                                    let forgot =
                                        self.drive_reconnects.lock().remove(route_id).is_some();
                                    self.drive_reconnect_inflight.lock().remove(route_id);
                                    if forgot {
                                        self.persist_drive_reconnects();
                                    }
                                }
                                self.process_effects(effects).await;
                                self.emit_snapshot();
                                return;
                            }
                        }
                    } else if let ControlMessage::Route(RouteControl::Reject { route_id, reason }) =
                        &msg
                    {
                        tracing::info!(
                            "inbound route reject for {route_id} from {}: {reason}",
                            short_id(&from)
                        );
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
                        ControlMessage::Route(RouteControl::VideoLane { route_id, lane }) => {
                            // The streamer told us which track lane this route's
                            // H.264 rides — record it so inbound samples demux to
                            // the right console window by binding, not by guess.
                            self.record_video_lane(&from, &route_id, lane);
                        }
                        ControlMessage::Route(RouteControl::DeadLane { media, lane }) => {
                            // A receiver says our media on that lane has no
                            // route on its side (it restarted and lost the
                            // name). Resolve the lane back to the route we
                            // pinned it to and fold it through the session as
                            // that route's Reject — stopping the encoder.
                            self.handle_dead_lane(&from, &media, lane).await;
                        }
                        ControlMessage::ProfileRequest => {
                            // A peer's refresh asks us to re-announce — send our
                            // current presence straight back so it re-learns us
                            // on the spot. The asker spaces these under its own
                            // backoff envelope, so we just answer.
                            tracing::debug!(
                                "presence re-announce requested by {}",
                                short_id(&from)
                            );
                            self.send_presence_to(&from).await;
                        }
                        msg => {
                            // A Reject landing on one of our client-side site
                            // mappings is the host saying its route is gone (a
                            // reconnect / network change tore it down). Grab the
                            // mapping now — the session's StopMedia is about to
                            // remove it — so we can auto-re-map it on the SAME
                            // local port and heal the tunnel with no unmap/remap.
                            // (A user-initiated unmap goes through disconnect(),
                            // never an inbound Reject, so this never fights a
                            // deliberate teardown.)
                            let heal_site = match &msg {
                                ControlMessage::Route(RouteControl::Reject {
                                    route_id, ..
                                }) => self
                                    .sites
                                    .mapping_details(route_id)
                                    .map(|d| (route_id.clone(), d)),
                                _ => None,
                            };
                            let effects = {
                                let mut st = self.state.lock();
                                st.session
                                    .as_mut()
                                    .map(|s| s.handle(NodeId::from(from.as_str()), msg))
                                    .unwrap_or_default()
                            };
                            self.process_effects(effects).await;
                            if let Some((old_route, (node, host_port, local_port))) = heal_site {
                                // Guarantee the dead route is fully cleared — a
                                // reject on a not-yet-active offer emits no
                                // StopMedia, so its mapping/listener would
                                // otherwise linger and block the re-map — then
                                // heal on the same local port off the hot path.
                                self.sites.stop_route(&old_route);
                                {
                                    let mut st = self.state.lock();
                                    if let Some(s) = st.session.as_mut() {
                                        let _ = s.teardown(&old_route);
                                    }
                                }
                                let mesh = self.clone();
                                crate::spawn(async move {
                                    mesh.remap_site_route(node, host_port, local_port).await;
                                });
                            }
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
                        match self.inbound_video_disposition(&frame.route, &from) {
                            InboundVideoDisposition::Accept => {}
                            InboundVideoDisposition::Pending => {
                                tracing::debug!(
                                    "early video frame for {} from {} dropped while route activation is pending",
                                    frame.route,
                                    short_id(&from)
                                );
                                return;
                            }
                            InboundVideoDisposition::Reject => {
                                tracing::debug!(
                                    "dropped video frame for {} from {} (route not live here)",
                                    frame.route,
                                    short_id(&from)
                                );
                                self.nack_dead_route(&from, &frame.route);
                                return;
                            }
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
                            // latest_wins: every JPEG is a complete picture, so
                            // an unread backlog is pure latency — supersede it.
                            // Without this the viewer replays history frame by
                            // frame ("always catching up") whenever decode or
                            // the wire runs behind the capture rate.
                            self.enqueue_for_watcher(&full.route, video_ipc_bytes(&full), true);
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
                        // on the mesh, so it takes two gates: a live input
                        // route from this exact sender, *and* the sender being
                        // authorized to drive this machine's control plane —
                        // its recorded owner, a co-owned fleet member, or a
                        // person the owner deliberately granted control to (the
                        // share path; without it a shared "Control" route
                        // activates but every event is dropped here). A CEC
                        // technician's authority is their customer's consent
                        // grant, which is evaluated at route *admission* and by
                        // the ~2s consent sweep — never per frame (see
                        // `sender_may_drive_admitted` / `spawn_cec_consent_sweep`):
                        // a lapsed grant tears the route down within a couple of
                        // seconds, so here a live CEC route just passes.
                        let route_ok = self.inbound_media_ok(&ev.route, &from, MediaKind::Input);
                        if route_ok && self.sender_may_drive_admitted(&from, DrivePlane::Input) {
                            self.injector.apply(&ev.route, ev.action);
                        } else {
                            // Refusing silently is how "controls just stopped
                            // working" went undiagnosable — say which gate
                            // failed, tell the viewer, and tell our own UI.
                            self.refuse_control_frame(&from, &ev.route, "input", route_ok);
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
            // CEC Support's own control channel (`cec.control`) — the
            // connect/approve/deny/end handshake. Distinct from AllMyStuff's
            // `CHANNEL_CONTROL` so CEC traffic never crosses into an ordinary
            // route negotiation.
            other if other == allmystuff_cec_protocol::CHANNEL_CONTROL => {
                tracing::info!("cec control in from {} on {network}", short_id(&from));
                self.handle_cec_control(from, network, payload).await;
            }
            // LEGACY CEC presence beacons — the transition path only. A
            // raised hand is asking-room membership now (signaling
            // presence), but a pre-asking-room customer build still
            // beacons `SupportPresence` on the standing area over a data
            // channel; feed those into the same queue (as TTL-governed
            // rows) so nobody's hand goes unseen mid-rollout. Beacons only
            // reach us over a live connection, so on the migrated Silent
            // area this arm quiesces as old builds age out — then it can be
            // deleted.
            other
                if other == allmystuff_cec_protocol::CHANNEL_PRESENCE
                    && network == allmystuff_cec_protocol::HELP_NETWORK_ID =>
            {
                let Ok(p) =
                    serde_json::from_value::<allmystuff_cec_protocol::SupportPresence>(payload)
                else {
                    return;
                };
                // The dialable number derives from the *authenticated* sender
                // id — never the payload — so a beacon can't park someone
                // else's number in the queue.
                let number = allmystuff_cec_protocol::support_id_from_device(&from);
                let changed = if p.available
                    && matches!(p.role, allmystuff_cec_protocol::Role::Client)
                {
                    tracing::info!("cec help beacon from {} (number {number})", short_id(&from));
                    self.cec
                        .record_help_beacon(&from, &number, &p.label, &p.hostname)
                } else {
                    self.cec.remove_help_beacon(&from)
                };
                if changed {
                    self.sink
                        .emit("cec://help", json!({ "waiting": self.cec.help_list() }));
                }
            }
            _ => {}
        }
    }

    /// Drop the per-route video state a route that just ended leaves behind —
    /// its receive-side counters, any pending re-key ask, its native decoder,
    /// the host-side pinned track lane (freeing it for the next stream), and
    /// the viewer-side lane→route binding.
    fn release_video_lanes(self: &Arc<Self>, route_id: &str) {
        self.note_video_route_stopped(route_id);
        self.video_in_stats.lock().remove(route_id);
        self.refresh_asks.lock().remove(route_id);
        self.video_decode.stop(route_id);
        // Invalidate queued capture callbacks before freeing the pin. Route
        // ids are stable across a re-offer; without this fence, old AUs can be
        // dequeued onto the successor's newly claimed lane.
        self.video_route_generations.lock().retire(route_id);
        // Host side: free the local pin so a later stream can reuse it. Keep
        // the daemon's fixed video track alive until its peer connection ends.
        // Closing it asynchronously here has an ABA race: a same-id re-offer
        // can reclaim lane N before the old MediaLaneClose arrives, and that
        // late close then destroys the replacement stream. Reusing the idle
        // fixed lane is both cheaper and generation-safe.
        self.video_lane_pins.lock().remove(route_id);
        // Viewer side: drop any lane binding that pointed at this route.
        let mut binds = self.video_lane_binds.lock();
        for per_peer in binds.values_mut() {
            per_peer.retain(|_, r| r != route_id);
        }
        binds.retain(|_, per_peer| !per_peer.is_empty());
    }

    /// Record both ends of a video route's local lifecycle. These timestamps
    /// never leave this process; they only recognize the tiny old-screen →
    /// new-screen handoff in which a duplicate close can otherwise kill the
    /// successor before its first capture frame.
    fn note_video_route_started(&self, route: &Route) {
        if !matches!(route.media, MediaKind::Display | MediaKind::Video) {
            return;
        }
        let Some(peer) = self.route_peer(&route.id) else {
            return;
        };
        self.video_switch_guards.lock().note_start(
            &route.id,
            &peer,
            route.to.as_str(),
            Instant::now(),
        );
    }

    fn note_video_route_stopped(&self, route_id: &str) {
        let facts = {
            let st = self.state.lock();
            st.session
                .as_ref()
                .and_then(|s| s.route(route_id))
                .filter(|r| matches!(r.route.media, MediaKind::Display | MediaKind::Video))
                .map(|r| (r.peer.as_str().to_string(), r.route.to.as_str().to_string()))
        };
        let Some((peer, sink)) = facts else { return };
        self.video_switch_guards
            .lock()
            .note_stop(route_id, &peer, &sink, Instant::now());
    }

    /// Read the narrow switch guard for an authenticated, currently-live
    /// display route. Call this before mutating the session to TornDown: after
    /// that mutation the replacement is indistinguishable from its predecessor
    /// by route id alone.
    fn take_early_video_teardown_guard(&self, route_id: &str) -> Option<VideoSwitchGuardHit> {
        let peer = {
            let st = self.state.lock();
            let route = st.session.as_ref()?.route(route_id)?;
            if !matches!(route.route.media, MediaKind::Display | MediaKind::Video)
                || route.state != RouteState::Active
            {
                return None;
            }
            route.peer.as_str().to_string()
        };
        self.video_switch_guards
            .lock()
            .take_early_teardown(route_id, &peer, Instant::now())
    }

    /// Cancel a quarantined close only when the same authenticated peer emits a
    /// periodic report for the active route after the in-flight-control floor.
    /// Feedback is an existing app control carried on the ICE data path; no new
    /// wire or signaling message is introduced.
    fn cancel_pending_video_teardown(&self, route_id: &str, from: &str) -> Option<u64> {
        let st = self.state.lock();
        let live = st
            .session
            .as_ref()
            .and_then(|s| s.route(route_id))
            .is_some_and(|route| {
                route.state == RouteState::Active
                    && matches!(route.route.media, MediaKind::Display | MediaKind::Video)
                    && pubkey_part(route.peer.as_str()) == pubkey_part(from)
            });
        live.then(|| {
            self.video_switch_guards
                .lock()
                .cancel_pending_on_mature_liveness(route_id, Instant::now())
        })
        .flatten()
    }

    /// A new local offer is an explicit replacement action, not a duplicate
    /// control delivery. Cancel its predecessor's delayed close before the
    /// local Session overwrites the deterministic route id.
    fn cancel_pending_video_teardown_replaced(&self, route_id: &str, from: &str) -> Option<u64> {
        let st = self.state.lock();
        let same_peer = st
            .session
            .as_ref()
            .and_then(|s| s.route(route_id))
            .is_some_and(|route| pubkey_part(route.peer.as_str()) == pubkey_part(from));
        same_peer
            .then(|| self.video_switch_guards.lock().cancel_pending(route_id))
            .flatten()
    }

    /// Apply a quarantined peer close after its grace period. This folds the
    /// original message through the session exactly once but deliberately does
    /// not call `disconnect`: echoing a new Teardown would turn a receive-side
    /// lifecycle decision into another wire message.
    async fn commit_quarantined_video_teardown(
        self: &Arc<Self>,
        route_id: String,
        from: String,
        network: String,
        token: u64,
        incarnation: u64,
    ) {
        tokio::time::sleep(VIDEO_INBOUND_TEARDOWN_QUARANTINE).await;
        let (effects, state_before, media) = {
            let mut st = self.state.lock();
            let Some(session) = st.session.as_mut() else {
                self.video_switch_guards.lock().take_pending_if_current(
                    &route_id,
                    token,
                    incarnation,
                );
                return;
            };
            let Some(route) = session.route(&route_id) else {
                self.video_switch_guards.lock().take_pending_if_current(
                    &route_id,
                    token,
                    incarnation,
                );
                return;
            };
            if route.state != RouteState::Active
                || !matches!(route.route.media, MediaKind::Display | MediaKind::Video)
                || pubkey_part(route.peer.as_str()) != pubkey_part(&from)
            {
                self.video_switch_guards.lock().take_pending_if_current(
                    &route_id,
                    token,
                    incarnation,
                );
                return;
            }
            if !self.video_switch_guards.lock().take_pending_if_current(
                &route_id,
                token,
                incarnation,
            ) {
                return;
            }
            let state_before = route.state.clone();
            let media = route.route.media;
            let effects = session.handle(
                NodeId::from(from.as_str()),
                ControlMessage::Route(RouteControl::Teardown {
                    route_id: route_id.clone(),
                }),
            );
            (effects, state_before, media)
        };
        let generation = self.video_route_generations.lock().current(&route_id);
        tracing::warn!(
            route = %route_id,
            from = %short_id(&from),
            network = %network,
            state_before = ?state_before,
            media = ?media,
            generation = ?generation,
            token,
            incarnation,
            disposition = "quarantine_expired_commit",
            "inbound route teardown"
        );
        self.process_effects(effects).await;
        self.emit_snapshot();
    }

    /// Allocate this actual `StartMedia` effect's process-local incarnation.
    /// The session state machine suppresses duplicate starts under its lock;
    /// therefore every effect reaching this boundary is a real start and must
    /// supersede any same-id predecessor even when a stale stop was correctly
    /// ignored before it could retire the old generation.
    fn begin_video_generation(&self, route_id: &str) -> u64 {
        let mut generations = self.video_route_generations.lock();
        let (generation, replaced) = generations.begin(route_id);
        if let Some(old) = replaced {
            tracing::warn!(
                "video route generation {old} replaced by {generation} for same-id successor {route_id}"
            );
        } else {
            tracing::info!("video route generation {generation} started for {route_id}");
        }
        generation
    }

    fn video_generation_is_current(&self, route_id: &str, generation: u64) -> bool {
        self.video_route_generations
            .lock()
            .is_current(route_id, generation)
    }

    /// Ask the daemon to close a media lane toward `peer` — the
    /// lifecycle-side of the pinned-lane free above. Spawned + logged:
    /// teardown must never block on the daemon, and a failure only means
    /// the lane idles until the connection ends (the pre-0.2.34 world).
    fn close_daemon_media_lane(self: &Arc<Self>, peer: &str, kind: &'static str, lane: u8) {
        let Some(network) = self.network_for_peer(peer) else {
            return;
        };
        let client = self.client.clone();
        let peer = pubkey_part(peer).to_string();
        crate::spawn(async move {
            match client
                .request(&Request::MediaLaneClose {
                    network,
                    peer: peer.clone(),
                    kind: kind.to_string(),
                    lane,
                })
                .await
            {
                Ok(resp) if resp.ok => {
                    tracing::debug!("closed {kind} lane {lane} toward {}", short_id(&peer));
                }
                Ok(resp) => {
                    tracing::debug!(
                        "daemon declined {kind} lane close (older daemon?): {:?}",
                        resp.error
                    );
                }
                Err(e) => tracing::debug!("{kind} lane close failed: {e}"),
            }
        });
    }

    /// The audio twin of [`Self::release_video_lanes`]: drop the route's
    /// Opus decoder when it ends — and close the daemon audio lane the
    /// end freed. Audio lanes are positional, not pinned like video
    /// ([`Self::audio_lane`]: a route streams on its rank among the
    /// peer's live Opus routes), so the lane that goes idle is the old
    /// **top** one — every survivor above the ended route shifts down —
    /// which is why the index closed here is the survivor count, not the
    /// ended route's own rank. A survivor mid-shift or a new route racing
    /// in is fine: the daemon (≥ 0.2.35) drains a closed lane for a grace
    /// window and a write inside it revives the track with no SDP work.
    fn release_audio_lanes(self: &Arc<Self>, route_id: &str) {
        self.audio_decoders.lock().remove(route_id);
        let Some(peer) = self.route_peer(route_id) else {
            return;
        };
        let cap = self.effective_audio_lanes(&peer);
        if cap == 0 {
            return;
        }
        // Excluding the ended route by id (rather than trusting it to be
        // gone) keeps the count right at every call site: teardown leaves
        // the route in the session map (only flipped to TornDown), and
        // the StopMedia effect can run either side of that flip.
        let survivors = self
            .sorted_media_routes(&peer, true, "opus")
            .iter()
            .filter(|id| id.as_str() != route_id)
            .count();
        // Lanes [0, survivors) are still owned; lane `survivors` is the
        // one this end vacated — when it had one at all. An overflow
        // route beyond the pool frees nothing (survivors >= cap), and
        // closing a lane that never opened is the daemon's idempotent
        // no-op, so a non-Opus route ending here costs nothing.
        if survivors < cap as usize {
            self.close_daemon_media_lane(&peer, "audio", survivors as u8);
        }
    }

    /// One Opus frame arrived on a peer's audio lane `stream`. It belongs
    /// to whichever of our routes maps to that lane (the lane-th Opus route
    /// from this peer in sorted order — [`Self::audio_route_for_lane`]),
    /// gated exactly like every other media frame (route live, sinks here,
    /// sender is the route's peer) — then decodes straight into the
    /// route's playback ring.
    fn handle_audio_inbound(self: &Arc<Self>, from: &str, stream: u8, data: Vec<u8>) {
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
            self.nack_dead_lane(from, "audio", stream);
            return;
        };
        self.clear_dead_lane(from, "audio", stream);
        if !self.inbound_media_ok(&route_id, from, MediaKind::Audio) {
            tracing::debug!("audio frame for {route_id} refused (route not live here)");
            self.nack_dead_route(from, &route_id);
            return;
        }
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
        data: Vec<u8>,
    ) {
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
            self.nack_dead_lane(from, "video", stream);
            return;
        };
        self.clear_dead_lane(from, "video", stream);
        match self.inbound_video_disposition(&route_id, from) {
            InboundVideoDisposition::Accept => {}
            InboundVideoDisposition::Pending => {
                if self.diag_ok(&format!("pending:{route_id}")) {
                    tracing::info!(
                        "early H.264 sample for {route_id} dropped during Offer→Accept; replacement route left intact"
                    );
                }
                return;
            }
            InboundVideoDisposition::Reject => {
                if self.diag_ok(&format!("gate:{route_id}")) {
                    tracing::warn!(
                        "H.264 samples for {route_id} refused — {}",
                        self.route_diag(&route_id, from)
                    );
                }
                self.nack_dead_route(from, &route_id);
                return;
            }
        }
        // The arrival side of the sender's "route active — streaming"
        // line: one INFO per stream, so a healthy hop is attributable
        // from this end too (the MJPEG path has logged its first frame
        // this way all along).
        let first = !self.video_in_stats.lock().contains_key(&route_id);
        if should_hold_first_video_sample(first, key, &data) {
            if self.diag_ok(&format!("entry:{route_id}")) {
                tracing::warn!(
                    "holding video deltas for {route_id} until a clean decode entry starts the current route generation"
                );
            }
            let mesh = self.clone();
            let refresh_route = route_id.clone();
            crate::spawn(async move {
                let _ = mesh.request_refresh(refresh_route).await;
            });
            return;
        }
        self.note_video_in(&route_id, "H.264", data.len());
        // Time the pacer's chunk trains as they land — the bandwidth
        // estimate + delay trend the feedback loop reports back (M3/T1.1).
        self.note_video_arrival(&route_id, rtp_timestamp, data.len());
        let (wants_decode, decoder_preference) = self
            .video_watchers
            .lock()
            .get(&route_id)
            .map(|w| (w.decode, w.decoder))
            .unwrap_or((false, DecoderPreference::Automatic));
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
                decoder_preference,
                Au { ts_us, key, data },
                move |packet| {
                    if let Some(mesh) = mesh.upgrade() {
                        mesh.enqueue_decoded(&rid, packet);
                    }
                },
                move |lost_ts_us| {
                    // The native decoder hit a corrupt unit or dumped its
                    // queue: name the broken AU in feedback (a capable
                    // sender heals with a GDR wave, no keyframe wall) AND
                    // keep the rate-limited re-key ask — old senders need
                    // it, and for new ones the wave lands first so the
                    // wall it forces is the same one today's path forced.
                    if let Some(mesh) = glitch_mesh.upgrade() {
                        let rid = glitch_rid.clone();
                        crate::spawn(async move {
                            if lost_ts_us.is_some() {
                                let _ = mesh
                                    .send_video_feedback(rid.clone(), 0, 1, 0, lost_ts_us)
                                    .await;
                            }
                            let _ = mesh.request_refresh(rid).await;
                        });
                    }
                },
            );
        } else {
            // NOT latest_wins: H.264 deltas must all reach the decoder in
            // order — freshest-wins happens after decode (enqueue_decoded) or
            // at the GUI's paint slot instead.
            self.enqueue_for_watcher(&route_id, h264_ipc_bytes(ts_us, key, &data), false);
        }
    }

    /// Queue one packet for a watching console window; drop the packet
    /// (with a debug note) when no window watches the route. A queue
    /// nobody drains (webview wedged or closing) caps at a few seconds
    /// of stream and is then cleared wholesale — the decoder re-keys on
    /// the sender's next IDR, and `video_unwatch`/route teardown remove
    /// the entry entirely.
    fn enqueue_for_watcher(&self, route_id: &str, packet: Vec<u8>, latest_wins: bool) {
        // One second of 60 fps H.264. An unread backlog is pure latency by
        // the time the window drains it (the old 120 held 2–4 s of "catch
        // up"), and the overflow below re-enters at a queued keyframe
        // instead of dumping the lot, so a slow-but-alive window loses
        // moments, not the stream.
        const MAX_QUEUED: usize = 60;
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
        if latest_wins {
            // Self-contained frames (MJPEG): anything the window hasn't
            // pulled yet is stale the moment a newer picture exists —
            // painting history buys nothing but lag. Mirrors
            // enqueue_decoded's freshest-wins.
            w.queue.clear();
        } else if w.queue.len() >= MAX_QUEUED {
            // H.264 backlog: skip forward to the NEWEST queued key unit —
            // decode re-enters there cleanly, and the viewer jumps to
            // near-live instead of replaying the whole backlog. The ipc
            // header carries the key flag (kind 2 at byte 0, key at byte
            // 1). No queued key (one delta chain longer than the cap, or
            // a keyless backlog): the old wholesale clear, recovering on
            // the sender's next IDR.
            match w
                .queue
                .iter()
                .rposition(|p| p.first() == Some(&2) && p.get(1) == Some(&1))
            {
                Some(i) if i > 0 => {
                    tracing::debug!(
                        "video queue for {route_id} unread — skipped {i} stale packets to its newest keyframe"
                    );
                    w.queue.drain(..i);
                }
                Some(_) => {
                    // The only key is already at the front and the queue is
                    // still at cap — the chain itself outgrew the bound.
                    tracing::debug!("video queue for {route_id} unread for a second — cleared");
                    w.queue.clear();
                }
                None => {
                    tracing::debug!("video queue for {route_id} unread and keyless — cleared");
                    w.queue.clear();
                }
            }
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

    /// Share one explicitly selected local folder as a real native drive on
    /// another machine. The absolute source path stays local and is bound to
    /// the unique route id before the offer leaves this process.
    pub async fn drive_map(
        self: &Arc<Self>,
        target: String,
        root: String,
        label: String,
        mount: String,
    ) -> Result<String, String> {
        self.drive_map_requested(target, root, label, mount, None)
            .await
    }

    async fn drive_map_requested(
        self: &Arc<Self>,
        target: String,
        root: String,
        label: String,
        mount: String,
        request: Option<String>,
    ) -> Result<String, String> {
        let me = self.local_node_id().ok_or("mesh not ready")?;
        let target = pubkey_part(&node_of(&target)).to_string();
        if target.is_empty() || same_node(&target, &me) {
            return Err("choose another machine for this drive".into());
        }
        let owner_or_fleet = self.sender_may_control(&target);
        let dialed_customer = self.cec.is_dialed(&target);
        let active_support = self.cec.has_active_session_with(&target);
        if request.is_none()
            && !drive_push_may_start(owner_or_fleet, dialed_customer, active_support)
        {
            return Err(
                "mapping a drive to another device requires Fleet or an active support session"
                    .into(),
            );
        }
        let root = std::path::PathBuf::from(root)
            .canonicalize()
            .map_err(|error| format!("couldn't open that folder: {error}"))?;
        if !root.is_dir() {
            return Err("choose a drive or folder".into());
        }
        let mut random = [0u8; 8];
        getrandom::getrandom(&mut random)
            .map_err(|error| format!("couldn't create a drive route: {error}"))?;
        let nonce = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let from = format!("{me}:drive-map:{nonce}");
        let to = format!("{target}:storage-in");
        let route = Route {
            id: format!("route:{from}→{to}"),
            from: from.into(),
            to: to.into(),
            media: MediaKind::Storage,
        };
        self.files.map_root(&route.id, root.clone());
        let drive = DriveRouteOffer {
            label: if label.trim().is_empty() {
                root.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "Remote drive".into())
            } else {
                label.trim().to_string()
            },
            root: Some(root.to_string_lossy().into_owned()),
            mount: mount.trim().to_string(),
            request,
        };
        let message = {
            let mut state = self.state.lock();
            let session = state.session.as_mut().ok_or("mesh not ready")?;
            session.offer_with_drive(
                route.clone(),
                target.as_str(),
                Vec::new(),
                Vec::new(),
                None,
                Some(drive),
            )
        };
        if let Err(error) = self.send_control(&target, &message).await {
            self.files.stop(&route.id);
            if let Some(session) = self.state.lock().session.as_mut() {
                let _ = session.teardown(&route.id);
            }
            return Err(error);
        }
        tracing::info!(
            "drive {} offered to {} from {}",
            route.id,
            short_id(&target),
            root.display()
        );
        self.emit_snapshot();
        Ok(route.id)
    }

    pub fn native_drives(&self) -> Vec<crate::drive_mount::NativeDriveInfo> {
        self.drive_mounts.list()
    }

    fn kvm_attached_to(&self, kvm: &str) -> Option<String> {
        self.state
            .lock()
            .session
            .as_ref()?
            .peers()
            .find(|profile| same_node(profile.node.as_str(), kvm))?
            .kvm
            .as_ref()?
            .attached_to
            .as_ref()
            .map(ToString::to_string)
    }

    /// KVM appliances whose authoritative binding points at this local node.
    /// A machine can technically have more than one attached appliance, so the
    /// support bridge treats this as a set rather than silently picking one.
    fn locally_attached_kvms(&self) -> Vec<String> {
        let Some(me) = self.local_node_id() else {
            return Vec::new();
        };
        let st = self.state.lock();
        st.session
            .as_ref()
            .map(|session| {
                session
                    .peers()
                    .filter(|profile| {
                        profile
                            .kvm
                            .as_ref()
                            .and_then(|kvm| kvm.attached_to.as_ref())
                            .is_some_and(|attached| same_node(attached.as_str(), &me))
                    })
                    .map(|profile| profile.node.to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Renew the two short leases behind customer-computer -> attached-KVM
    /// passthrough. The customer tells its technician which KVM to expect, then
    /// tells that KVM which technician may drive it. The appliance independently
    /// verifies that this sender is its recorded attached computer.
    async fn refresh_kvm_support(&self) {
        const LEASE_SECS: u64 = 8;
        let kvms = self.locally_attached_kvms();
        if kvms.is_empty() {
            return;
        }
        for technician in self.cec.active_session_technicians() {
            for kvm in &kvms {
                // Discovery goes first so the KVM's immediate targeted greet
                // cannot race the technician's strict CEC presence filter.
                let available = ControlMessage::App(AppControl::KvmSupportAvailable {
                    kvm: NodeId::from(kvm.clone()),
                    expires_in: LEASE_SECS,
                });
                if let Err(error) = self.send_control(&technician, &available).await {
                    tracing::debug!(
                        "couldn't announce support KVM {} to technician {}: {error}",
                        short_id(kvm),
                        short_id(&technician)
                    );
                    continue;
                }
                let grant = ControlMessage::App(AppControl::KvmSupportGrant {
                    technician: technician.clone(),
                    expires_in: LEASE_SECS,
                });
                if let Err(error) = self.send_control(kvm, &grant).await {
                    tracing::debug!(
                        "couldn't delegate technician {} to attached KVM {}: {error}",
                        short_id(&technician),
                        short_id(kvm)
                    );
                }
            }
        }
    }

    /// Remove a transient KVM profile after its customer heartbeat lapses. A
    /// KVM that is also ours/shared or directly dialed remains: those are
    /// independent relationships and must not be torn down with passthrough.
    async fn drop_transient_support_kvm(self: &Arc<Self>, kvm: &str) {
        if self.cec.relationship_with(kvm) || self.peer_has_durable_non_cec_standing(kvm) {
            return;
        }
        let (effects, dropped) = {
            let mut st = self.state.lock();
            st.peer_networks.remove(kvm);
            st.peer_features.remove(kvm);
            st.peer_links.remove(kvm);
            st.peer_boots.remove(kvm);
            let mut effects = Vec::new();
            let mut dropped = false;
            if let Some(session) = st.session.as_mut() {
                let node = {
                    session
                        .peers()
                        .find(|profile| same_node(profile.node.as_str(), kvm))
                        .map(|profile| profile.node.clone())
                };
                if let Some(node) = node {
                    effects.extend(session.drop_peer(&node));
                    dropped = true;
                }
            }
            (effects, dropped)
        };
        if dropped {
            tracing::info!(
                "CEC support KVM {} is no longer delegated — removing its transient graph profile",
                short_id(kvm)
            );
            self.process_effects(effects).await;
            self.emit_snapshot();
        }
    }

    /// Stage local install/firmware media on a KVM's USB mass-storage gadget.
    pub async fn kvm_media_stage(
        self: &Arc<Self>,
        kvm: String,
        path: String,
        label: String,
    ) -> Result<(), String> {
        let me = self.local_node_id().ok_or("mesh not ready")?;
        let kvm = pubkey_part(&node_of(&kvm)).to_string();
        if kvm.is_empty() || same_node(&kvm, &me) {
            return Err("choose a KVM for virtual media".into());
        }
        if self
            .kvm_attached_to(&kvm)
            .is_some_and(|attached| same_node(&attached, &me))
        {
            return Err(
                "the KVM's attached computer cannot also source its virtual media; choose another fleet/shared/technician machine"
                    .into(),
            );
        }
        let port = self.site_map(kvm.clone(), 80).await?;
        crate::kvm_media::stage(port, &me, &path, &label).await?;
        // Mount metadata is now part of KVM presence. Ask for a fresh advert
        // so every graph learns the source→KVM relationship immediately.
        let _ = self
            .send_control(&kvm, &ControlMessage::ProfileRequest)
            .await;
        Ok(())
    }

    /// Ask another Files-authorized machine to source the media. The source
    /// performs the upload itself, so large images never bounce through this
    /// controller or its webview.
    pub async fn kvm_media_stage_from(
        self: &Arc<Self>,
        source: String,
        kvm: String,
        path: String,
        label: String,
    ) -> Result<(), String> {
        let me = self.local_node_id().ok_or("mesh not ready")?;
        let source = pubkey_part(&node_of(&source)).to_string();
        if source.is_empty() || same_node(&source, &me) {
            return self.kvm_media_stage(kvm, path, label).await;
        }
        if self
            .kvm_attached_to(&kvm)
            .is_some_and(|attached| same_node(&attached, &source))
        {
            return Err("the KVM's attached computer cannot source its virtual media".into());
        }
        let mut random = [0u8; 16];
        getrandom::getrandom(&mut random)
            .map_err(|error| format!("couldn't create a KVM media request: {error}"))?;
        let request = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let (acknowledged, ack) = oneshot::channel();
        {
            let mut requests = self.kvm_media_requests.lock();
            requests.retain(|_, pending| pending.made.elapsed() < Duration::from_secs(60 * 60 * 7));
            requests.insert(
                request.clone(),
                KvmMediaRequest {
                    source: source.clone(),
                    kvm: kvm.clone(),
                    label: label.clone(),
                    made: Instant::now(),
                    acknowledged: Some(acknowledged),
                },
            );
        }
        if let Err(error) = self
            .send_control(
                &source,
                &ControlMessage::App(AppControl::StageKvmMedia {
                    request: request.clone(),
                    kvm,
                    path,
                    label,
                }),
            )
            .await
        {
            self.kvm_media_requests.lock().remove(&request);
            return Err(error);
        }
        match tokio::time::timeout(Duration::from_secs(15), ack).await {
            Ok(Ok(())) => Ok(()),
            _ => {
                self.kvm_media_requests.lock().remove(&request);
                Err("the source didn't accept virtual-media staging (it may need an AllMyStuff update)".into())
            }
        }
    }

    pub async fn kvm_media_unmount(self: &Arc<Self>, kvm: String) -> Result<(), String> {
        let kvm = pubkey_part(&node_of(&kvm)).to_string();
        if kvm.is_empty() {
            return Err("choose a KVM".into());
        }
        let port = self.site_map(kvm.clone(), 80).await?;
        crate::kvm_media::unmount(port).await?;
        let _ = self
            .send_control(&kvm, &ControlMessage::ProfileRequest)
            .await;
        Ok(())
    }

    /// Ask another authorized machine to expose `root` back to us. This is
    /// the inbound-map twin of `drive_map`; the source re-checks Files access
    /// and canonicalizes the path on its own filesystem before offering.
    pub async fn drive_map_from(
        self: &Arc<Self>,
        source: String,
        root: String,
        label: String,
        mount: String,
    ) -> Result<(), String> {
        let source = pubkey_part(&node_of(&source)).to_string();
        if source.is_empty()
            || self
                .local_node_id()
                .is_some_and(|me| same_node(&source, &me))
        {
            return Err("choose another machine as the drive source".into());
        }
        let mut random = [0u8; 16];
        getrandom::getrandom(&mut random)
            .map_err(|error| format!("couldn't create a drive request: {error}"))?;
        let request = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        self.drive_pull_tokens
            .lock()
            .retain(|_, pull| pull.made.elapsed() < Duration::from_secs(120));
        self.drive_pull_tokens.lock().insert(
            request.clone(),
            DrivePullRequest {
                source: source.clone(),
                root: root.clone(),
                label: label.clone(),
                mount: mount.clone(),
                made: Instant::now(),
            },
        );
        if let Err(error) = self
            .send_control(
                &source,
                &ControlMessage::App(AppControl::MapDrive {
                    root,
                    label,
                    mount,
                    request: request.clone(),
                }),
            )
            .await
        {
            self.drive_pull_tokens.lock().remove(&request);
            return Err(error);
        }
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if !self.drive_pull_tokens.lock().contains_key(&request) {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        self.drive_pull_tokens.lock().remove(&request);
        Err("the source didn't offer that drive (it may need an AllMyStuff update, or the selected path is unavailable)".into())
    }

    fn persist_drive_reconnects(&self) {
        let mappings = self.drive_reconnects.lock();
        if !persist_drive_reconnects(&self.drive_reconnect_path, &mappings) {
            tracing::error!("couldn't save native drive mappings");
        }
    }

    /// Rebuild receiver-initiated drive routes whose source has returned with
    /// a fresh app/session incarnation. The Windows drive letter is briefly
    /// unavailable while the old route is reaped, then is claimed again with
    /// the same label and mount instead of disappearing permanently.
    fn reconnect_drive_pulls(self: &Arc<Self>, peer: &str) {
        let canonical = pubkey_part(peer);
        let intents: Vec<(String, DriveReconnect)> = self
            .drive_reconnects
            .lock()
            .iter()
            .filter(|(_, intent)| same_node(&intent.source, canonical))
            .map(|(route, intent)| (route.clone(), intent.clone()))
            .collect();
        if intents.is_empty() {
            return;
        }
        let active: std::collections::HashSet<String> = {
            let st = self.state.lock();
            intents
                .iter()
                .filter(|(route, _)| {
                    st.session
                        .as_ref()
                        .and_then(|session| session.route(route))
                        .is_some_and(|route| route.is_active())
                })
                .map(|(route, _)| route.clone())
                .collect()
        };
        for (old_route, intent) in intents {
            if active.contains(&old_route)
                || !self
                    .drive_reconnect_inflight
                    .lock()
                    .insert(old_route.clone())
            {
                continue;
            }
            let mesh = self.clone();
            crate::spawn(async move {
                // StopMedia unmaps asynchronously. Let Windows release the old
                // WebDAV letter before offering its replacement.
                tokio::time::sleep(Duration::from_millis(1500)).await;
                let mut last_error = None;
                for retry_delay in [0u64, 3_000, 8_000] {
                    if retry_delay != 0 {
                        tokio::time::sleep(Duration::from_millis(retry_delay)).await;
                    }
                    match mesh
                        .drive_map_from(
                            intent.source.clone(),
                            intent.root.clone(),
                            intent.label.clone(),
                            intent.mount.clone(),
                        )
                        .await
                    {
                        Ok(()) => {
                            mesh.drive_reconnects.lock().remove(&old_route);
                            mesh.persist_drive_reconnects();
                            last_error = None;
                            tracing::info!(
                                "native drive from {} reconnected after the source returned",
                                short_id(&intent.source)
                            );
                            break;
                        }
                        Err(error) => last_error = Some(error),
                    }
                }
                mesh.drive_reconnect_inflight.lock().remove(&old_route);
                if let Some(error) = last_error {
                    tracing::warn!(
                        "native drive from {} is still unavailable after reconnect attempts: {error}",
                        short_id(&intent.source)
                    );
                }
            });
        }
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
        let video = if video.is_empty() {
            video
        } else {
            // This list is a one-shot decision for the whole session —
            // wait out the bring-up race before stripping anything.
            self.await_video_bringup().await;
            if self.daemon_video.load(Ordering::SeqCst) {
                video
            } else {
                Vec::new()
            }
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
        // Self / loopback is decided by *canonical* node id: the route's
        // endpoints carry the suffixed display id the UI built them from,
        // while `me` is the bare node id, so a raw `==` would miss a genuine
        // self-route and offer it over the wire (where it never returns) —
        // which is exactly what stopped local terminals from opening.
        let from_is_me = same_node(&from_node, &me);
        let to_is_me = same_node(&to_node, &me);
        let peer = if from_is_me { to_node } else { from_node };

        if from_is_me && to_is_me {
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

        if matches!(route.media, MediaKind::Display | MediaKind::Video) {
            if let Some(token) =
                self.cancel_pending_video_teardown_replaced(&route.id, peer.as_str())
            {
                tracing::warn!(
                    route = %route.id,
                    peer = %short_id(peer.as_str()),
                    token,
                    disposition = "quarantine_canceled_by_local_reoffer",
                    "local video route control"
                );
            }
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
    pub fn video_watch(&self, route_id: String, decode: bool, decoder: DecoderPreference) -> u64 {
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
        tracing::info!(
            "window watching {route_id} (native decode: {decode}, decoder: {decoder:?})"
        );
        // A fresh watch (a re-open, an input switch) must start its peer's
        // video dead-lane grace over. When the previous session closed, its
        // orphaned frames drained with no route here and left a stale
        // "unmapped since" mark for that peer's lane. Without this reset, the
        // very first frame of the NEW stream — arriving in the brief gap before
        // the route is mapped — sees that elapsed grace and NACKs the sender
        // instantly, whose handle_dead_lane then StopMedia's the capture we
        // just restarted: the "reconnect shows nothing / connecting forever"
        // loop, and it bit every video route, not just CEC. Clearing only on a
        // *new* watch keeps a genuine close (no re-watch) NACKing as before.
        //
        // Derive the peer straight from the route id (`route:{from}→{to}`), NOT
        // the session: at watch time the route often isn't registered yet (the
        // offer and this watch land in the same tick — the daemon logs both at
        // the same millisecond), so route_peer would return None and silently
        // skip the reset. For an inbound video route the `from` end is the
        // streaming peer, which is exactly what nack_dead_lane keys on.
        if let Some(from_cap) = route_id
            .strip_prefix("route:")
            .and_then(|s| s.split_once('→'))
            .map(|(from, _)| from)
        {
            let peer_node = node_of(from_cap);
            let prefix = format!("deadlane:video:{}:", pubkey_part(&peer_node));
            self.dead_lane_since
                .lock()
                .retain(|k, _| !k.starts_with(&prefix));
        }
        self.video_watchers.lock().insert(
            route_id,
            VideoWatcher {
                token,
                decode,
                decoder,
                queue: std::collections::VecDeque::new(),
                last_poll: Instant::now(),
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
        w.last_poll = Instant::now();
        let total: usize = w.queue.iter().map(|p| 4 + p.len()).sum();
        let mut out = Vec::with_capacity(total);
        for packet in w.queue.drain(..) {
            out.extend_from_slice(&(packet.len() as u32).to_le_bytes());
            out.extend_from_slice(&packet);
        }
        out
    }

    pub async fn disconnect(self: &Arc<Self>, route_id: String) -> Result<(), String> {
        // An explicit disconnect means "forget this mapping", not merely
        // "this connection incarnation ended". Automatic peer-restart reaps
        // bypass this command and intentionally retain the reconnect intent.
        let forgot = self.drive_reconnects.lock().remove(&route_id).is_some();
        self.drive_reconnect_inflight.lock().remove(&route_id);
        if forgot {
            self.persist_drive_reconnects();
        }
        if let Some(hit) = self.take_early_video_teardown_guard(&route_id) {
            // Do not infer intent from watcher *presence*: unwatch is a
            // separate fire-and-forget command and can lag. A window that polls
            // after this disconnect began is positive proof the successor is
            // still live; an intentional close stops its 16 ms poll loop even
            // if cleanup delivery itself is delayed.
            let guarded_at = Instant::now();
            tokio::time::sleep(
                VIDEO_SWITCH_TEARDOWN_GUARD
                    .saturating_sub(hit.age)
                    .max(VIDEO_LOCAL_POLL_OBSERVE),
            )
            .await;
            let polled_after_request = self
                .video_watchers
                .lock()
                .get(&route_id)
                .is_some_and(|watcher| watcher_poll_proves_liveness(watcher.last_poll, guarded_at));
            let still_nonterminal = self
                .state
                .lock()
                .session
                .as_ref()
                .and_then(|s| s.route(&route_id))
                .is_some_and(|r| {
                    matches!(
                        r.state,
                        RouteState::Offered | RouteState::Incoming | RouteState::Active
                    )
                });
            if polled_after_request && still_nonterminal {
                tracing::warn!(
                    "ignored stale local video teardown for {route_id} after switch from {} — the successor window kept polling",
                    hit.predecessor,
                );
                return Ok(());
            }
            tracing::info!(
                "early local video teardown for {route_id} confirmed after switch from {} (no successor poll) — committing",
                hit.predecessor,
            );
        }
        let msg = {
            let mut st = self.state.lock();
            st.session.as_mut().and_then(|s| s.teardown(&route_id))
        };
        tracing::info!("local route teardown committing for {route_id}");
        self.audio.stop(&route_id);
        self.video.stop(&route_id);
        self.video_watchers.lock().remove(&route_id);
        self.release_video_lanes(&route_id);
        self.release_audio_lanes(&route_id);
        self.terminal.stop(&route_id);
        self.files.stop(&route_id);
        self.drive_mounts.stop(&route_id);
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
        // A CEC customer a technician dialed is an ordinary mesh peer here, with
        // no special grouping: the CEC area is Silent (no roster), so there is no
        // "fleet" to seat it under. Strangers merely co-resident on the CEC
        // rooms never reach this session at all — the presence gate
        // (`cec::is_cec_network` + `Cec::relationship_with`) admits only
        // deliberate CEC peers, so the graph can't fill with the support
        // area's other customers. The CEC tab lists dialed customers from CEC
        // state (`cec_dialed`), not from the graph.
        let peers: Vec<Value> = session
            .peers()
            .map(|p| serde_json::to_value(p).unwrap_or(Value::Null))
            .collect();
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

    // ---- CEC Support (technician + customer) --------------------------
    //
    // CEC Support rides this exact engine, but its public Silent rooms are
    // signaling directories, never data meshes. Customers announce in
    // `cecsupport-clients`; technicians lurk listen-only. Raising a hand is
    // joining `cecsupport-asking`, whose presence is the technicians' queue.
    // The app suppresses those public sightings outside the support workflow.
    // A technician answers a raised hand (or a phoned-in number) by joining
    // the selected customer's `cec-<Support ID>` room and deliberately dialing
    // them there — a direct session, with TURN as WebRTC's NAT fallback, never
    // a mesh hop. From then on that customer is an
    // ordinary AllMyStuff graph peer with the normal screen/control
    // features. The only substitution is trust — a CEC route is authorized
    // by the customer's live consent grant ([`crate::cec`]) rather than
    // owner/fleet, checked per frame in [`Self::sender_may_drive`] so a
    // revoke bites mid-session. Every command mirrors the node-control
    // surface the CEC client app and this app's CEC tab both depend on
    // verbatim.

    /// `cec_status`: this node's CEC snapshot — its own support number (a
    /// display label), the public support directory, its role, and whether the
    /// technician's help-queue view is armed.
    pub async fn cec_status(&self) -> Result<Value, String> {
        let me = self.resolve_local_id().await;
        let mut status = self.cec.status(me.as_deref());
        if let Some(o) = status.as_object_mut() {
            // The technician's "watch the help queue" opt-in — a view state
            // the node holds, surfaced so the Support tab's toggle reflects it
            // across a reload.
            o.insert(
                "help_watching".into(),
                Value::Bool(self.cec.watching_help()),
            );
        }
        Ok(status)
    }

    /// `cec_online` (customer): announce in the public `cecsupport-clients`
    /// directory and host this customer's isolated session room. The public
    /// membership is standing and admission-disabled: technicians can discover
    /// the customer but cannot make it a data mesh. Joining raises no hand —
    /// that's `cec_ask_help`'s job (membership
    /// in the sibling asking room). Returns `{ number }` for the app's
    /// display: the digits a customer reads over the phone, derived from the
    /// device key.
    pub async fn cec_online(self: &Arc<Self>) -> Result<Value, String> {
        let me = self
            .resolve_local_id()
            .await
            .ok_or_else(|| "this device has no mesh identity yet".to_string())?;
        let number = self.cec.own_number(Some(&me));
        let (network_id, config) = crate::cec::help_network_config();
        self.cec_join_silent(&network_id, config).await?;
        self.cec_prepare_directory_area().await;
        // The global area above is discovery-only. Host the actual support
        // transport in this customer's own room so unrelated CEC nodes can
        // never become data peers.
        let (session_network, session_config) = crate::cec::session_network_config(&number);
        self.cec_join_silent(&session_network, session_config)
            .await?;
        self.cec_restore_dialed_session_rooms().await;
        self.cec_sweep_stale_asking_room().await;
        tracing::info!(
            "CEC Support: discoverable on {network_id}; hosting isolated session room {session_network} as number {number}"
        );
        Ok(json!({ "number": number }))
    }

    /// Rejoin and re-pin only the customers in this technician's durable CEC
    /// directory. These isolated rooms replace the old pins on the global
    /// support area without losing wake/reboot reconnect behavior.
    async fn cec_restore_dialed_session_rooms(self: &Arc<Self>) {
        for record in self.cec.dialed_records() {
            let canonical = crate::cec::pubkey_part(&record.node).to_string();
            let (network, config) = crate::cec::session_network_config(&record.number);
            if let Err(e) = self.cec_join_silent(&network, config).await {
                tracing::warn!(
                    "CEC Support: couldn't restore session room {network} for {}: {e}",
                    short_id(&canonical)
                );
                continue;
            }
            let _ = self
                .client
                .request(&Request::NetworkConnectPeer {
                    network,
                    peer: canonical,
                    pin: true,
                    wait_ms: 0,
                })
                .await;
        }
    }

    /// Enforce the global support area's only valid shape: signaling-only,
    /// approval-gated, with no pins, roster, or live transports. Older builds
    /// pinned support sessions here, so merely changing the config would leave
    /// those authenticated WebRTC links alive. A dirty area is purged and
    /// recreated; actual sessions are restored in per-customer rooms instead.
    async fn cec_prepare_directory_area(self: &Arc<Self>) {
        let (area, config) = crate::cec::help_network_config();

        let config_dirty = match self.client.request(&Request::ConfigShow).await {
            Ok(r) if r.ok => r
                .data
                .as_ref()
                .and_then(|d| d.pointer("/config/networks"))
                .and_then(Value::as_array)
                .and_then(|nets| {
                    nets.iter().find(|n| {
                        n.get("id").and_then(Value::as_str) == Some(area.as_str())
                            || n.get("network_id").and_then(Value::as_str) == Some(area.as_str())
                    })
                })
                .is_some_and(|cfg| {
                    cfg.get("auto_approve").and_then(Value::as_bool) != Some(false)
                        || cfg
                            .get("pinned_peers")
                            .and_then(Value::as_array)
                            .is_some_and(|pins| !pins.is_empty())
                }),
            _ => false,
        };
        let kind_dirty = match self
            .client
            .request(&Request::GovernanceState {
                network: area.clone(),
            })
            .await
        {
            Ok(r) if r.ok => {
                r.data
                    .as_ref()
                    .and_then(|d| d.pointer("/state/kind"))
                    .and_then(Value::as_str)
                    != Some("silent")
            }
            _ => false,
        };
        let roster_dirty = match self
            .client
            .request(&Request::RosterList {
                network: area.clone(),
            })
            .await
        {
            Ok(r) if r.ok => r
                .data
                .as_ref()
                .and_then(|d| d.get("roster"))
                .and_then(Value::as_array)
                .is_some_and(|entries| !entries.is_empty()),
            _ => false,
        };
        let transport_dirty = match self
            .client
            .request(&Request::PeersList {
                network: area.clone(),
            })
            .await
        {
            Ok(r) if r.ok => r
                .data
                .as_ref()
                .and_then(|d| d.get("peers"))
                .and_then(Value::as_array)
                .is_some_and(|peers| {
                    peers.iter().any(|peer| {
                        !matches!(
                            peer.get("status").and_then(Value::as_str),
                            Some("sighted" | "offline") | None
                        )
                    })
                }),
            _ => false,
        };

        if !(config_dirty || kind_dirty || roster_dirty || transport_dirty) {
            return;
        }
        tracing::warn!(
            config_dirty,
            kind_dirty,
            roster_dirty,
            transport_dirty,
            "CEC Support: purging data links and legacy state from the discovery-only area"
        );
        let _ = self
            .client
            .request(&Request::NetworkRemove {
                network: area.clone(),
                purge: true,
            })
            .await;
        if let Err(e) = self.cec_join_silent(&area, config).await {
            tracing::warn!("CEC Support: re-join of the directory area failed: {e}");
        }
        self.sync_networks().await;
    }

    /// Whether a peer has a reason to stay in the AllMyStuff catalog even if
    /// an obsolete CEC area roster also names it. These stores are durable and
    /// authoritative; unlike the support roster, they express deliberate
    /// user relationships.
    fn peer_has_durable_non_cec_standing(&self, peer: &str) -> bool {
        let canonical = pubkey_part(peer);
        self.ownership
            .owner()
            .is_some_and(|owner| pubkey_part(&owner) == canonical)
            || self
                .ownership
                .any_fleet_member(|member| pubkey_part(member) == canonical)
            || self.fleet_authorized.lock().contains(canonical)
            || self.shares.person_for_node(canonical).is_some()
    }

    /// Leave the asking room if this node has no live reason to be in it —
    /// the hand isn't up and the queue view isn't armed. The daemon persists
    /// CEC rooms and auto-rejoins them at startup, so a crash mid-ask (or a
    /// technician exiting with the watch armed) would otherwise leave this
    /// device sitting in the queue room forever, reading as a raised hand to
    /// every watching technician.
    async fn cec_sweep_stale_asking_room(self: &Arc<Self>) {
        if self.cec.asking_help() || self.cec.watching_help() {
            return;
        }
        let joined = {
            let st = self.state.lock();
            st.networks
                .iter()
                .any(|n| n == allmystuff_cec_protocol::ASK_NETWORK_ID)
        };
        if !joined {
            return;
        }
        tracing::info!("CEC Support: leaving a stale asking-room membership (no live ask/watch)");
        let _ = self
            .client
            .request(&Request::NetworkRemove {
                network: allmystuff_cec_protocol::ASK_NETWORK_ID.to_string(),
                purge: true,
            })
            .await;
        self.sync_networks().await;
    }

    /// `cec_ask_help { on }` (customer): raise the hand — **join the asking
    /// room**. The room is Silent, so joining opens no connections; this
    /// device simply becomes present in the queue room's signaling, which is
    /// the entire "I need help" signal a watching technician sees (their
    /// queue is the room's member list, and the dialable number derives from
    /// this device's announced id). The technician answers by dialing this
    /// device in its isolated session room. Presence carries want, never access: a
    /// session still takes the full consent handshake.
    pub async fn cec_ask_help(self: &Arc<Self>, on: bool) -> Result<Value, String> {
        let me = self
            .resolve_local_id()
            .await
            .ok_or_else(|| "this device has no mesh identity yet".to_string())?;
        if on {
            // Idempotent — bring-up already joined the standing area; a hand
            // raised before the first `cec_online` (or after a manual mesh
            // removal) self-heals.
            let _ = self.cec_online().await;
            let (ask_room, config) = crate::cec::ask_network_config();
            self.cec_join_silent(&ask_room, config).await?;
            self.cec.set_asking_help(true);
            // The room join IS the raised hand: the engine announces on the
            // room's signaling the moment it attaches, and every watching
            // technician's queue picks the presence up. Nothing to re-send,
            // no wires to race — `raised` tells the waiting card the hand is
            // genuinely up (the join round-tripped through the daemon).
            self.sink
                .emit("cec://help", json!({ "asking": true, "raised": true }));
            tracing::info!("CEC Support: asking for help — joined {ask_room}");
            let _ = me;
        } else {
            self.cec_stop_asking_help().await;
        }
        Ok(json!({ "asking": on }))
    }

    /// Withdraw the help ask: **leave the asking room**, which removes this
    /// device from every watching technician's queue at once (the daemon
    /// broadcasts a signaling Leave; a crash instead ages out with the
    /// room's presence). Shared by the explicit cancel and the automatic
    /// clear when a session gets approved — help arrived, stop asking for
    /// it. The standing directory and isolated session room are untouched. A
    /// technician whose own hand
    /// was up while their queue view is armed doesn't leave — they drop
    /// back to the listen-only watch join instead.
    async fn cec_stop_asking_help(self: &Arc<Self>) {
        if !self.cec.set_asking_help(false) {
            return;
        }
        if self.cec.watching_help() {
            let (ask_room, config) = crate::cec::ask_watch_network_config();
            let _ = self.cec_join_silent(&ask_room, config).await;
        } else {
            let _ = self
                .client
                .request(&Request::NetworkRemove {
                    network: allmystuff_cec_protocol::ASK_NETWORK_ID.to_string(),
                    purge: true,
                })
                .await;
            self.sync_networks().await;
        }
        // Tell this customer's own UI (the CEC Support app's waiting card) —
        // the automatic clear on approval otherwise leaves it looking armed.
        self.sink.emit("cec://help", json!({ "asking": false }));
        tracing::info!("CEC Support: no longer asking for help");
    }

    /// `cec_help_watch { on }` (technician): arm or disarm the help-queue
    /// view. Arming joins both public rooms listen-only; disarming leaves the
    /// asking-room view and clears the queue. Existing customer sessions remain
    /// in their isolated rooms, so stopping the queue never hangs them up.
    pub async fn cec_help_watch(self: &Arc<Self>, on: bool) -> Result<Value, String> {
        if on {
            let (network_id, config) = crate::cec::help_watch_network_config();
            self.cec_join_silent(&network_id, config).await?;
            self.cec_prepare_directory_area().await;
            let (_, watcher_config) = crate::cec::help_watch_network_config();
            self.cec_join_silent(&network_id, watcher_config).await?;
            self.cec_restore_dialed_session_rooms().await;
            // Listen-only: the queue is read by presence without announcing
            // presence — a watching technician must not read as a raised
            // hand in other technicians' queues.
            let (ask_room, ask_config) = crate::cec::ask_watch_network_config();
            self.cec_join_silent(&ask_room, ask_config).await?;
            self.cec.set_watching_help(true);
            // Seed the queue from the room's current members — hands raised
            // before this technician started watching must not wait for
            // their next announce to appear.
            self.cec_reconcile_help_queue().await;
            tracing::info!("CEC Support: watching the help queue on {ask_room}");
        } else {
            self.cec.set_watching_help(false);
            self.cec.clear_help();
            self.cec_sweep_stale_asking_room().await;
            self.sink.emit("cec://help", json!({ "waiting": [] }));
            tracing::info!("CEC Support: stopped watching the help queue");
        }
        Ok(json!({ "watching": on }))
    }

    /// `cec_help_list` (technician): the customers currently waiting,
    /// longest-waiting first. Reconciles the queue against the asking room's
    /// live member list on the way (the tab polls this, so a dropped
    /// presence event heals within a beat), but never *joins* the room —
    /// that's `cec_help_watch`'s job, an explicit opt-in, so merely opening
    /// the tab can't silently sign a node up for the queue.
    pub async fn cec_help_list(self: &Arc<Self>) -> Result<Value, String> {
        self.cec_reconcile_help_queue().await;
        Ok(Value::Array(self.cec.help_list()))
    }

    /// Reconcile the presence-sourced help queue against the asking room's
    /// live member list (every status counts — Sighted IS the raised hand on
    /// a Silent room). No-op while the watch view is disarmed.
    async fn cec_reconcile_help_queue(self: &Arc<Self>) {
        if !self.cec.watching_help() {
            return;
        }
        let me = self.local_node_id().map(|m| pubkey_part(&m).to_string());
        let Ok(resp) = self
            .client
            .request(&Request::PeersList {
                network: allmystuff_cec_protocol::ASK_NETWORK_ID.to_string(),
            })
            .await
        else {
            return;
        };
        let Some(peers) = resp
            .data
            .as_ref()
            .and_then(|d| d.get("peers"))
            .and_then(|p| p.as_array())
        else {
            return;
        };
        let present: std::collections::HashSet<String> = peers
            .iter()
            .filter_map(|p| p.get("device_id").and_then(|v| v.as_str()))
            .map(|id| pubkey_part(id).to_string())
            // Defensive self-filter for older daemons that ignore listen_only.
            .filter(|id| me.as_deref() != Some(id.as_str()))
            .collect();
        if self.cec.help_sync_presence(&present) {
            self.sink
                .emit("cec://help", json!({ "waiting": self.cec.help_list() }));
        }
    }

    /// `cec_dial` (technician): the dial-by-number fallback — the digits a
    /// customer reads over the phone, for when the raised-hand list is too
    /// crowded to spot them (or they just prefer saying a number). Resolves
    /// the digits to a device id **on the support area** — a raised hand
    /// first (the beacon's authenticated sender), else any area member whose
    /// key-derived number matches — then joins that customer's private room and
    /// dials it like any answered hand. Returns `{ node }`.
    pub async fn cec_dial(
        self: &Arc<Self>,
        number: String,
        agent_name: String,
    ) -> Result<Value, String> {
        let digits = crate::cec::number_digits(&number);
        if digits.len() != 9 {
            return Err(format!(
                "'{number}' isn't a support number (9 digits, spacing optional)"
            ));
        }
        // The area is where customers are — be on it before looking.
        let (area, config) = crate::cec::help_watch_network_config();
        self.cec_join_silent(&area, config).await?;
        self.cec_prepare_directory_area().await;
        let (_, watcher_config) = crate::cec::help_watch_network_config();
        self.cec_join_silent(&area, watcher_config).await?;
        let node = match self.cec.help_seeker_by_number(&digits) {
            Some(node) => node,
            None => self
                .cec_member_by_number(&area, &digits)
                .await
                .ok_or_else(|| {
                    format!(
                        "no customer with number {} is on the support area right now — \
                     have them open CEC Support (or raise their hand) and try again",
                        crate::cec::grouped_number(&digits)
                    )
                })?,
        };
        self.cec_dial_node(node, agent_name).await
    }

    /// Scan the support area's member list for the device whose key-derived
    /// support number matches `digits`. Presence-level (Sighted counts) — the
    /// customer doesn't need a connection to be found, just to be alive on
    /// the area.
    async fn cec_member_by_number(&self, area: &str, digits: &str) -> Option<String> {
        let resp = self
            .client
            .request(&Request::PeersList {
                network: area.to_string(),
            })
            .await
            .ok()?;
        let peers = resp.data?.get("peers")?.as_array()?.to_owned();
        peers.iter().find_map(|p| {
            let id = p.get("device_id")?.as_str()?;
            (allmystuff_cec_protocol::support_id_from_device(id) == digits).then(|| id.to_string())
        })
    }

    /// `cec_dial_node` (technician): open a support session with `node` in the
    /// customer's isolated room — the headline path, fed straight from a raised
    /// hand's beacon (its authenticated device id), and the tail of the
    /// dial-by-number fallback. Pins the connection (a support session is a
    /// standing dial), records the customer in the device-keyed directory,
    /// and sends the consent connect-request stamped with `agent_name`.
    /// Returns `{ node }`.
    pub async fn cec_dial_node(
        self: &Arc<Self>,
        node: String,
        agent_name: String,
    ) -> Result<Value, String> {
        if !agent_name.trim().is_empty() {
            self.cec.set_agent_name(agent_name.clone());
        }
        let agent_name = if agent_name.trim().is_empty() {
            self.cec.agent_name()
        } else {
            agent_name
        };
        self.cec.note_technician();
        let customer = node;
        let canonical = crate::cec::pubkey_part(&customer).to_string();
        let number = allmystuff_cec_protocol::support_id_from_device(&customer);
        let (network_id, config) = crate::cec::session_network_config(&number);
        self.cec_join_silent(&network_id, config).await?;
        // The row is directory-worthy from the moment of the dial — emitted
        // immediately so the CEC tab shows it right away; the post-connect
        // refresh below fills in the live ident.
        let (label, hostname) = self.cec_peer_ident(&canonical).unwrap_or_default();
        let attempt =
            self.cec
                .record_dialed(customer.clone(), number.clone(), label, hostname, false);
        self.sink.emit("cec://peer", attempt.to_value());
        let cancel = self.cec.begin_dial();
        self.client
            .request(&Request::NetworkConnectPeer {
                network: network_id.clone(),
                peer: canonical.clone(),
                // A support session is a standing dial: the daemon redials
                // this customer on every announce (the Silent room's one
                // exception) and never ages the intent out — the far end
                // sleeping, roaming, or rebooting no longer kills the
                // relationship. Persisted daemon-side with the network.
                pin: true,
                wait_ms: 0,
            })
            .await
            .map_err(|e| e.to_string())
            .and_then(|resp| {
                if resp.ok {
                    Ok(())
                } else {
                    Err(resp
                        .error
                        .unwrap_or_else(|| "connect_peer refused by the daemon".into()))
                }
            })?;

        let (label, hostname) = self.cec_peer_ident(&canonical).unwrap_or_default();
        let record =
            self.cec
                .record_dialed(customer.clone(), number.clone(), label, hostname, true);
        tracing::info!(
            "CEC Support: dialed customer {} in isolated room {network_id}",
            short_id(&customer),
        );

        // The connect handshake — the customer's node raises the 3-choice
        // prompt from this. `connect_peer` above only *initiates* the WebRTC
        // connection; the daemon's acknowledged-delivery contract does the
        // rest: the Request is queued until the customer's link is up,
        // retransmitted across session rebuilds, and the reply resolves only
        // when the customer's node has actually taken the frame — the 2s
        // retransmit loop this used to need is the daemon's job now. The
        // send rides a spawned task so the dial returns immediately; a
        // delivery failure (TTL, terminal drop) marks the session ended so
        // the GUI's waiting badge tells the truth instead of hanging.
        let session_id = format!("cec-{}-{}", short_id(&customer), fresh_boot_id());
        let want_control = true;
        self.cec.set_session(&session_id, "requested");
        let request = allmystuff_cec_protocol::ControlMessage::Connect(
            allmystuff_cec_protocol::ConnectControl::Request {
                session_id: session_id.clone(),
                agent_name,
                want_control,
            },
        );
        {
            let mesh = self.clone();
            let net = network_id.clone();
            let peer = canonical.clone();
            let sid = session_id.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                match mesh
                    .cec_send_control_acked(&net, &peer, &request, CEC_CONNECT_TTL)
                    .await
                {
                    Ok(()) => {
                        tracing::info!(
                            "CEC Support: connect request delivered to {}",
                            short_id(&peer)
                        );
                    }
                    Err(e) => {
                        // "Stop trying" beat us here, or delivery genuinely
                        // lapsed — either way the waiting badge clears.
                        if !cancel.load(std::sync::atomic::Ordering::Relaxed) {
                            tracing::warn!("cec connect request undelivered: {e}");
                        }
                        mesh.cec.set_session(&sid, "ended");
                        mesh.sink.emit(
                            "cec://session",
                            json!({ "session_id": sid, "state": "ended" }),
                        );
                    }
                }
            });
        }
        self.sink.emit("cec://peer", record.to_value());
        self.sink.emit(
            "cec://session",
            json!({ "session_id": session_id, "state": "requested" }),
        );
        self.emit_snapshot();
        Ok(json!({ "node": customer }))
    }

    /// `cec_cancel_dial` (technician): stop whatever the in-flight dial is
    /// trying — the discovery poll and the connect-request re-send loop both
    /// quit at the flag. The attempt row stays (the directory is permanent);
    /// a no-dial-in-flight cancel is a harmless no-op.
    pub async fn cec_cancel_dial(self: &Arc<Self>) -> Result<Value, String> {
        self.cec.cancel_dial();
        Ok(Value::Null)
    }

    /// `cec_pending` (customer): the inbound connect-requests awaiting a choice.
    pub async fn cec_pending(&self) -> Result<Value, String> {
        Ok(Value::Array(self.cec.pending()))
    }

    /// `cec_approve` (customer): record the chosen `scope` grant for `tech` and
    /// drive the mesh approval so the session goes Active. The grant is what the
    /// per-frame gate then consults, so the technician's screen/input rides the
    /// normal engine.
    pub async fn cec_approve(
        self: &Arc<Self>,
        tech: String,
        scope: String,
        session_id: String,
        want_control: bool,
    ) -> Result<Value, String> {
        let scope = crate::cec::parse_scope(&scope)?;
        let agent_name = self.cec.pending_agent_name(&tech);
        self.cec.approve(&tech, &agent_name, scope, want_control)?;
        self.cec.set_session(&session_id, "active");
        // Bind the session to this technician so the consent sweep can end
        // exactly their sessions when the grant later lapses.
        self.cec.bind_session(&session_id, &tech);
        // Prime attached-KVM discovery and authority now, rather than making
        // the newly approved console wait for the next two-second renewal
        // sweep before its KVM screen can be selected.
        self.refresh_kvm_support().await;
        let canonical = crate::cec::pubkey_part(&tech).to_string();
        if let Some(network_id) = self.network_for_peer(&tech) {
            self.cec_send_decision(
                network_id,
                canonical.clone(),
                allmystuff_cec_protocol::ControlMessage::Connect(
                    allmystuff_cec_protocol::ConnectControl::Approve {
                        session_id: session_id.clone(),
                        scope,
                    },
                ),
            );
        }
        // Carry `tech`/`agent_name` on the event (like the auto-approve path
        // does), so the customer GUI can bind the session — and its chat — to
        // this technician even when no `cec://request` preceded it.
        self.sink.emit(
            "cec://session",
            json!({
                "session_id": session_id,
                "state": "active",
                "tech": tech,
                "agent_name": agent_name,
            }),
        );
        self.cec_emit_grants();
        // Help arrived — an approved session withdraws the ask automatically,
        // so the customer never has to remember they raised their hand.
        if self.cec.asking_help() {
            self.cec_stop_asking_help().await;
        }
        Ok(Value::Null)
    }

    /// `cec_deny` (customer): decline a pending request (no grant recorded).
    pub async fn cec_deny(
        self: &Arc<Self>,
        tech: String,
        session_id: String,
    ) -> Result<Value, String> {
        self.cec.deny(&tech);
        self.cec.set_session(&session_id, "denied");
        let canonical = crate::cec::pubkey_part(&tech).to_string();
        if let Some(network_id) = self.network_for_peer(&tech) {
            self.cec_send_decision(
                network_id,
                canonical.clone(),
                allmystuff_cec_protocol::ControlMessage::Connect(
                    allmystuff_cec_protocol::ConnectControl::Deny {
                        session_id: session_id.clone(),
                        reason: "declined".into(),
                    },
                ),
            );
        }
        self.sink.emit(
            "cec://session",
            json!({ "session_id": session_id, "state": "denied" }),
        );
        Ok(Value::Null)
    }

    /// `cec_chat_send` (either side): send one live chat line to `peer` over the
    /// existing CEC session, then echo it into our own transcript. Chat is
    /// live-only — it rides the `cec.control` channel of a session that already
    /// exists, so with no known network/route to the peer there is nothing to
    /// carry it and this errs (the GUI only offers chat inside a live session).
    pub async fn cec_chat_send(
        self: &Arc<Self>,
        peer: String,
        text: String,
    ) -> Result<Value, String> {
        let canonical = crate::cec::pubkey_part(&peer).to_string();
        let network = self
            .network_for_peer(&peer)
            .ok_or_else(|| "no live CEC session with this peer to carry chat".to_string())?;
        // `from` is THIS node's own side of the session, which is what the far
        // GUI aligns the bubble from. We are the technician exactly when we
        // dialed this peer (they sit in our dialed-customer directory);
        // otherwise we are the customer who answered a request — the two sides a
        // CEC session ever has. The wire message's own `from` is never trusted
        // for the peer key, only for rendering.
        let from = if self.cec.is_dialed(&canonical) {
            allmystuff_cec_protocol::Role::Technician
        } else {
            allmystuff_cec_protocol::Role::Client
        };
        let msg = allmystuff_cec_protocol::ChatMessage {
            id: fresh_chat_id(),
            from,
            text,
            ts: crate::cec::now_secs(),
        };
        // Send chat over the very same peer-to-peer path as everything else on
        // the session (the connect handshake, presence, roster): direct over the
        // P2P link, the topology's forwarders only if there's no direct edge. The
        // acked/reliable path was the wrong tool — its per-peer outbox flushes
        // ONLY over a direct link and *parks* the frame when that link isn't up
        // (an ICE flap drops `data_channel_open`), so on the hub-shaped CEC area a
        // technician's line could sit unsent instead of just going P2P like the
        // rest of the session.
        self.cec_send_control(
            &network,
            &canonical,
            &allmystuff_cec_protocol::ControlMessage::Chat(msg.clone()),
        )
        .await?;
        tracing::info!(
            "cec chat out to {} ({} chars) on {network}",
            short_id(&canonical),
            msg.text.chars().count()
        );
        // Append + echo our own line so the sender's history is complete and the
        // GUI has ONE render path (the `cec://chat` event) for sent and received
        // alike.
        self.cec.push_chat(&canonical, msg.clone());
        self.emit_cec_chat(&canonical, &msg);
        Ok(json!({ "id": msg.id, "ts": msg.ts }))
    }

    /// `cec_chat_history` (either side): the persisted transcript with `peer`,
    /// oldest-first, as `{ messages: [ { id, from, text, ts } ] }` — what a GUI
    /// loads when it opens the chat pane. Both sent and received lines are here,
    /// since a sent line is echoed into the store on the way out.
    pub async fn cec_chat_history(self: &Arc<Self>, peer: String) -> Result<Value, String> {
        let canonical = crate::cec::pubkey_part(&peer).to_string();
        let messages: Vec<Value> = self
            .cec
            .chat_history(&canonical)
            .iter()
            .map(|m| serde_json::to_value(m).unwrap_or(Value::Null))
            .collect();
        Ok(json!({ "messages": messages }))
    }

    /// `cec_revoke` (customer): "Forget this technician" — drop every grant and
    /// tear the session down. The consent revoke bites the next privileged frame
    /// even if this wire End is lost.
    pub async fn cec_revoke(self: &Arc<Self>, tech: String) -> Result<Value, String> {
        let removed = self.cec.revoke(&tech)?;
        let canonical = crate::cec::pubkey_part(&tech).to_string();
        if let Some(network_id) = self.network_for_peer(&tech) {
            self.cec_send_decision(
                network_id,
                canonical.clone(),
                allmystuff_cec_protocol::ControlMessage::Connect(
                    allmystuff_cec_protocol::ConnectControl::End {
                        session_id: String::new(),
                    },
                ),
            );
        }
        // Tear down any live routes with the technician, exactly like forgetting
        // a node.
        self.teardown_and_drop_peer(&canonical).await;
        self.cec_emit_grants();
        Ok(json!({ "revoked": removed }))
    }

    /// `cec_grants` (customer): the live consent grants.
    pub async fn cec_grants(&self) -> Result<Value, String> {
        Ok(Value::Array(self.cec.grants()))
    }

    /// `cec_dialed` (technician): the customers this node has dialed, for the
    /// CEC tab's "Active connections" list. Dialed customers are ordinary graph
    /// peers — this is CEC state, not a graph grouping.
    pub async fn cec_dialed(&self) -> Result<Value, String> {
        // The dialed directory is durable — it survives node restarts and grant
        // expiry, so a technician keeps every machine they've serviced. Reconcile
        // each entry's `online` against the daemon's live peer set so the tab
        // shows which stored machines are reachable right now (and can be
        // reconnected — an expired grant just re-prompts the customer).
        let records = self.cec.dialed_records();
        // Each stored customer has a distinct session room, so reconcile
        // reachability against that room's peer set.
        let mut out = Vec::with_capacity(records.len());
        for r in records {
            let canonical = crate::cec::pubkey_part(&r.node).to_string();
            let network = allmystuff_cec_protocol::network_id_for_number(&r.number);
            let reachable = self.cec_reachable_set(&network).await;
            let online = reachable.contains(&canonical);
            if online != r.online {
                self.cec.set_customer_online(&canonical, online);
            }
            let mut v = r.to_value();
            v["online"] = json!(online);
            out.push(v);
        }
        Ok(Value::Array(out))
    }

    /// `forget_node` — an **app-wide** feature on every node's gear, not a CEC
    /// one: drop `node` from the graph + roster and tear its live routes down.
    /// Any AllMyStuff node can forget any peer this way. When the peer happens to
    /// be a CEC customer this technician dialed (or a CEC technician this
    /// customer approved), [`Self::cec_forget_cleanup`] also unwinds that CEC
    /// state — but the core teardown is identical for every node.
    pub async fn forget_node(self: &Arc<Self>, node: String) -> Result<Value, String> {
        let canonical = crate::cec::pubkey_part(&node).to_string();
        // App-wide: tear down live routes to the peer and drop it from the
        // roster on whatever network it was reachable on.
        self.teardown_and_drop_peer(&canonical).await;
        // CEC add-on: a no-op for an ordinary node.
        self.cec_forget_cleanup(&node, &canonical).await;
        self.emit_snapshot();
        Ok(json!({ "forgotten": node }))
    }

    // ---- CEC internals ------------------------------------------------

    /// CEC-specific cleanup layered onto [`Self::forget_node`] — a no-op for an
    /// ordinary (non-CEC) peer. Revokes any grant for `node` (customer side)
    /// and drops the dialed record (technician side), including the customer's
    /// isolated room so its durable pin cannot reconnect later.
    async fn cec_forget_cleanup(self: &Arc<Self>, node: &str, canonical: &str) {
        let dialed_room = self
            .cec
            .dialed_records()
            .into_iter()
            .find(|record| pubkey_part(&record.node) == canonical)
            .map(|record| allmystuff_cec_protocol::network_id_for_number(&record.number));
        let _ = self.cec.forget_dialed(canonical);
        // On the technician side the room belongs to exactly this customer.
        // Leaving it is also the only daemon-0.3.3 way to remove its persisted
        // standing dial, so "Forget" really stops reconnecting.
        if let Some(network) = dialed_room {
            let _ = self
                .client
                .request(&Request::NetworkRemove {
                    network,
                    purge: true,
                })
                .await;
            self.sync_networks().await;
        }
        // Customer side: forgetting a technician is also a revoke.
        let _ = self.cec.revoke(node);
        self.cec_emit_grants();
    }

    /// Join a Silent mesh via the daemon and re-subscribe this session's
    /// channels onto it.
    async fn cec_join_silent(
        self: &Arc<Self>,
        network_id: &str,
        config: Value,
    ) -> Result<(), String> {
        let resp = self
            .client
            .request(&Request::NetworkAdd {
                config: config.clone(),
            })
            .await
            .map_err(|e| e.to_string())?;
        if !resp.ok {
            let err = resp.error.unwrap_or_default();
            // The daemon persists CEC rooms and auto-rejoins them at startup, so
            // a re-host (or re-dial) hits "config id already in use" — that's
            // success, not failure: we ARE on the room. Treating it as an error
            // made `cec_start_hosting` bail before advertising presence (the
            // customer then never shows up as a host) and blocked a re-dial from
            // refreshing the room. Any *other* failure is still real.
            if err.contains("already in use") || err.contains("already joined") {
                // A persisted room keeps its persisted *config* — push the
                // current one over it so label / signaling drift (e.g. the
                // watcher's listen-only join replacing an announcing one)
                // heals in place. The network *kind* is NOT healable this
                // way: it's governed state bootstrapped at first attach, so
                // an area still governed `open` (the beacon-era shape) is
                // purged and re-created by `cec_prepare_directory_area`, not
                // updated. A failed update degrades to the old behavior
                // rather than failing the join.
                let _ = self
                    .client
                    .request(&Request::NetworkUpdate { config })
                    .await;
                self.sync_networks().await;
                return Ok(());
            }
            return Err(if err.is_empty() {
                format!("couldn't join the CEC mesh {network_id}")
            } else {
                err
            });
        }
        self.sync_networks().await;
        Ok(())
    }

    /// Whether `canonical` (bare pubkey) is currently a peer on `network_id`,
    /// per the daemon's `PeersList` — the live-reachability check behind a stored
    /// customer's online dot. A daemon error or a network we've left reads as
    /// offline (best-effort; the row stays, it just shows unreachable).
    /// The set of canonical (bare-pubkey) ids **connected** on `network_id`
    /// right now. "Reachable" is the `active`/`shelved` cut the graph reads
    /// online from — an offline / sighted / handshaking row is a peer the
    /// daemon remembers, not one a technician can reach, so it must not read as
    /// online. (The old per-id check ignored status, so a still-listed but
    /// offline customer read "online" until the app restarted.)
    async fn cec_reachable_set(&self, network_id: &str) -> std::collections::HashSet<String> {
        let mut set = std::collections::HashSet::new();
        let Ok(resp) = self
            .client
            .request(&Request::PeersList {
                network: network_id.to_string(),
            })
            .await
        else {
            return set;
        };
        let Some(peers) = resp
            .data
            .as_ref()
            .and_then(|d| d.get("peers"))
            .and_then(|p| p.as_array())
        else {
            return set;
        };
        for p in peers {
            if !status_is_reachable(p.get("status").and_then(|v| v.as_str())) {
                continue;
            }
            if let Some(id) = p.get("device_id").and_then(|v| v.as_str()) {
                set.insert(crate::cec::pubkey_part(id).to_string());
            }
        }
        set
    }

    /// Fire a customer *decision* (Approve / Deny / End) at a technician
    /// under the acked contract, without blocking the GUI op that made
    /// the decision: the send is spawned, queued daemon-side until the
    /// technician's link is up, retransmitted across rebuilds, and a
    /// terminal delivery failure is logged loudly — the one case left is
    /// a technician gone past the TTL, who re-dials and (for standing
    /// grants) auto-approves without the customer doing anything.
    fn cec_send_decision(
        self: &Arc<Self>,
        network: String,
        peer: String,
        message: allmystuff_cec_protocol::ControlMessage,
    ) {
        let mesh = self.clone();
        crate::spawn(async move {
            if let Err(e) = mesh
                .cec_send_control_acked(
                    &network,
                    &peer,
                    &message,
                    std::time::Duration::from_secs(30),
                )
                .await
            {
                tracing::warn!(
                    "cec decision undelivered to {} (they can re-dial; standing grants auto-approve): {e}",
                    short_id(&peer)
                );
            }
        });
    }

    /// Send one CEC [`ControlMessage`](allmystuff_cec_protocol::ControlMessage)
    /// under the daemon's acknowledged-delivery contract: queued until the
    /// peer's link is up, retransmitted across session rebuilds, resolved
    /// when the peer's node has taken the frame (or errs at `ttl`). The
    /// client read deadline is sized past the TTL so the daemon's honest
    /// timeout answer always wins over the socket's.
    async fn cec_send_control_acked(
        &self,
        network: &str,
        peer: &str,
        message: &allmystuff_cec_protocol::ControlMessage,
        ttl: std::time::Duration,
    ) -> Result<(), String> {
        let payload = serde_json::to_value(message).map_err(|e| e.to_string())?;
        let resp = self
            .client
            .request_with_timeout(
                &Request::ChannelSendReliable {
                    network: network.to_string(),
                    channel: allmystuff_cec_protocol::CHANNEL_CONTROL.to_string(),
                    peer: crate::cec::pubkey_part(peer).to_string(),
                    payload,
                    ttl_ms: ttl.as_millis() as u64,
                },
                ttl + std::time::Duration::from_secs(5),
            )
            .await
            .map_err(|e| e.to_string())?;
        if resp.ok {
            Ok(())
        } else {
            Err(resp
                .error
                .unwrap_or_else(|| "cec acked control send failed".into()))
        }
    }

    /// Send one CEC [`ControlMessage`](allmystuff_cec_protocol::ControlMessage)
    /// on the `cec.control` channel to `peer` (bare pubkey) on `network`.
    async fn cec_send_control(
        &self,
        network: &str,
        peer: &str,
        message: &allmystuff_cec_protocol::ControlMessage,
    ) -> Result<(), String> {
        let payload = serde_json::to_value(message).map_err(|e| e.to_string())?;
        let resp = self
            .client
            .request(&Request::ChannelSendTo {
                network: network.to_string(),
                channel: allmystuff_cec_protocol::CHANNEL_CONTROL.to_string(),
                peer: crate::cec::pubkey_part(peer).to_string(),
                payload,
            })
            .await
            .map_err(|e| e.to_string())?;
        if resp.ok {
            Ok(())
        } else {
            Err(resp
                .error
                .unwrap_or_else(|| "cec control send failed".into()))
        }
    }

    // (`cec_broadcast_presence` — the `SupportPresence` channel beacon — is
    // gone: a raised hand is asking-room membership now, carried entirely by
    // signaling. The inbound `cec.presence` handler stays for one transition
    // window so pre-asking-room customers still land in the queue.)

    /// The best-known (label, hostname) for a peer by canonical id, from the
    /// live session — the identity pair the CEC cards spell out so the
    /// technician's row and the customer's own app match word for word.
    fn cec_peer_ident(&self, canonical: &str) -> Option<(String, String)> {
        let st = self.state.lock();
        let session = st.session.as_ref()?;
        let ident = session
            .peers()
            .find(|p| crate::cec::pubkey_part(p.node.as_str()) == canonical)
            .map(|p| (p.label.clone(), p.hostname.clone()))
            .filter(|(l, h)| !l.is_empty() || !h.is_empty());
        ident
    }

    /// Tear down every live route with a peer (by canonical id) and drop it from
    /// the daemon roster on whatever network it was reachable on — the shared
    /// body of the app-wide "Forget this node" and CEC's "Forget this technician".
    async fn teardown_and_drop_peer(self: &Arc<Self>, canonical: &str) {
        let route_ids: Vec<String> = {
            let st = self.state.lock();
            match st.session.as_ref() {
                Some(session) => session
                    .routes()
                    .filter(|r| crate::cec::pubkey_part(r.peer.as_str()) == canonical)
                    .map(|r| r.route.id.clone())
                    .collect(),
                None => Vec::new(),
            }
        };
        for id in route_ids {
            let _ = self.disconnect(id).await;
        }
        if let Some(network) = self.network_for_peer(canonical) {
            let _ = self
                .client
                .request(&Request::RosterRemove {
                    network,
                    device_id: canonical.to_string(),
                })
                .await;
        }
    }

    /// Emit the customer's current grant list (`cec://grants`).
    fn cec_emit_grants(&self) {
        self.sink
            .emit("cec://grants", json!({ "grants": self.cec.grants() }));
    }

    /// Handle one inbound CEC control message (the `cec.control` channel).
    /// Customer side: a `Request` raises the 3-choice prompt (`cec://request`);
    /// technician side: an `Approve`/`Deny`/`End` moves the session.
    async fn handle_cec_control(self: &Arc<Self>, from: String, network: String, payload: Value) {
        let msg: allmystuff_cec_protocol::ControlMessage = match serde_json::from_value(payload) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("dropping CEC control from {}: {e}", short_id(&from));
                return;
            }
        };
        // Chat rides the same control channel as the connect handshake but is a
        // pure transcript event, not part of the approve/deny state machine —
        // attribute it to the authenticated sender, store it, surface it, and
        // return before the Connect dispatch below (which consumes `msg`).
        if let allmystuff_cec_protocol::ControlMessage::Chat(chat) = &msg {
            self.on_cec_chat_in(&from, chat).await;
            return;
        }
        if let allmystuff_cec_protocol::ControlMessage::Connect(connect) = msg {
            tracing::info!(
                "cec connect message from {}: {}",
                short_id(&from),
                match &connect {
                    allmystuff_cec_protocol::ConnectControl::Request { session_id, .. } =>
                        format!("Request session={session_id}"),
                    allmystuff_cec_protocol::ConnectControl::Approve { session_id, .. } =>
                        format!("Approve session={session_id}"),
                    allmystuff_cec_protocol::ConnectControl::Deny { session_id, .. } =>
                        format!("Deny session={session_id}"),
                    allmystuff_cec_protocol::ConnectControl::End { session_id } =>
                        format!("End session={session_id}"),
                    other => format!("{other:?}"),
                }
            );
            match connect {
                allmystuff_cec_protocol::ConnectControl::Request {
                    session_id,
                    agent_name,
                    want_control,
                } => {
                    // The technician retransmits its Request every 2s until it
                    // sees an answer — because a single send can be dropped
                    // before the data channel is up (the very race the Request
                    // retransmit was added to beat). Our *reply* can be dropped
                    // the same way, so each incoming beat is our cue to re-assert
                    // our current decision, answered on the network it arrived
                    // on. Without this, an approval whose one Approve was dropped
                    // leaves the technician re-requesting forever (the customer
                    // re-prompted every beat) and never seeing the session.
                    match self.cec.session_state(&session_id).as_deref() {
                        Some("active") => {
                            // Already approved; re-send the Approve. The scope is
                            // cosmetic to the technician (its Approve handler only
                            // moves the session to active) — default it if the
                            // grant is gone.
                            let scope = self
                                .cec
                                .active_scope_for(&from)
                                .unwrap_or(allmystuff_cec_protocol::ApprovalScope::Once);
                            let _ = self
                                .cec_send_control(
                                    &network,
                                    &from,
                                    &allmystuff_cec_protocol::ControlMessage::Connect(
                                        allmystuff_cec_protocol::ConnectControl::Approve {
                                            session_id,
                                            scope,
                                        },
                                    ),
                                )
                                .await;
                        }
                        Some("denied") => {
                            // Already declined; re-send the Deny so the tech's
                            // dial loop can stop instead of re-prompting us.
                            let _ = self
                                .cec_send_control(
                                    &network,
                                    &from,
                                    &allmystuff_cec_protocol::ControlMessage::Connect(
                                        allmystuff_cec_protocol::ConnectControl::Deny {
                                            session_id,
                                            reason: "declined".into(),
                                        },
                                    ),
                                )
                                .await;
                        }
                        _ => {
                            // A still-valid standing grant (3-hours / Forever)
                            // auto-approves the reconnect — the customer set it so
                            // they wouldn't be re-asked, which is what lets a
                            // technician reuse a connection without the customer
                            // doing anything. An expired or absent grant (or an
                            // "Once" that never persisted) falls through to the
                            // prompt, so reconnecting to a lapsed machine pops the
                            // box again, exactly like the first time.
                            // Standing grants only: an "Approve Once" covers
                            // exactly its own session, so a *new* dial from a
                            // once-approved technician re-prompts instead of
                            // silently reattaching off the leftover grant.
                            if let Some(scope) = self.cec.standing_scope_for(&from) {
                                // Each dial mints a fresh session id — end any
                                // older live session with this same technician
                                // first, so a re-dial supersedes rather than
                                // piling "X is viewing your screen" rows up.
                                for stale in self.cec.end_other_sessions(&session_id) {
                                    self.sink.emit(
                                        "cec://session",
                                        json!({ "session_id": stale, "state": "ended" }),
                                    );
                                }
                                self.cec.set_session(&session_id, "active");
                                // Bind the auto-approved session to this
                                // technician so the consent sweep can end it
                                // when the standing grant later lapses.
                                self.cec.bind_session(&session_id, &from);
                                // A standing approval is already active, so
                                // make the attached KVM available before the
                                // approval response reaches the technician.
                                self.refresh_kvm_support().await;
                                if let Some(rec) =
                                    self.cec.touch_dialed(crate::cec::pubkey_part(&from))
                                {
                                    self.sink.emit("cec://peer", rec.to_value());
                                }
                                self.sink.emit(
                                    "cec://session",
                                    json!({
                                        "session_id": session_id.clone(),
                                        "state": "active",
                                        "agent_name": agent_name.clone(),
                                        "tech": from.clone(),
                                    }),
                                );
                                tracing::info!(
                                    "cec auto-approve: standing grant covers {} — replying Approve session={session_id}",
                                    short_id(&from)
                                );
                                // Acked: a new-era technician sends its Request
                                // exactly once, so this reply must survive drops
                                // on its own — the daemon queues, retransmits
                                // across rebuilds, and only gives up at the TTL.
                                // (An old technician re-beats; its duplicate
                                // Requests just re-spawn cheap dedup'd replies.)
                                self.cec_send_decision(
                                    network.clone(),
                                    from.clone(),
                                    allmystuff_cec_protocol::ControlMessage::Connect(
                                        allmystuff_cec_protocol::ConnectControl::Approve {
                                            session_id,
                                            scope,
                                        },
                                    ),
                                );
                                // Help arrived (a standing grant answered the
                                // raised hand) — withdraw the ask, same as an
                                // explicit approve does.
                                if self.cec.asking_help() {
                                    self.cec_stop_asking_help().await;
                                }
                            } else {
                                // Undecided: raise the prompt on the first beat and
                                // refresh the pending record on later ones — but
                                // don't re-emit `cec://request`, or the customer's
                                // approval dialog is spammed once every 2s.
                                let already = self.cec.has_pending_session(&session_id);
                                let verification_code =
                                    crate::cec::verification_code(&from, &session_id);
                                let req = crate::cec::PendingRequest {
                                    tech: from.clone(),
                                    agent_name: agent_name.clone(),
                                    want_control,
                                    session_id: session_id.clone(),
                                    verification_code: verification_code.clone(),
                                };
                                self.cec.record_pending(req);
                                if !already {
                                    self.sink.emit(
                                        "cec://request",
                                        json!({
                                            "tech": from,
                                            "agent_name": agent_name,
                                            "want_control": want_control,
                                            "session_id": session_id,
                                            "verification_code": verification_code,
                                        }),
                                    );
                                }
                            }
                        }
                    }
                }
                allmystuff_cec_protocol::ConnectControl::Approve { session_id, .. } => {
                    self.cec.set_session(&session_id, "active");
                    // Bind the technician-side session as well. This is the
                    // proof checked when the approved customer announces the
                    // KVM physically attached to it.
                    self.cec.bind_session(&session_id, &from);
                    // The customer just approved — this connection is now in
                    // active use. Stamp its `last_used` (and re-emit the peer so
                    // the CEC tab's time-since refreshes) so the technician's
                    // stale-connection cleanup reflects real activity.
                    if let Some(rec) = self.cec.touch_dialed(crate::cec::pubkey_part(&from)) {
                        self.sink.emit("cec://peer", rec.to_value());
                    }
                    self.sink.emit(
                        "cec://session",
                        json!({ "session_id": session_id, "state": "active" }),
                    );
                }
                allmystuff_cec_protocol::ConnectControl::Deny { session_id, .. } => {
                    self.cec.set_session(&session_id, "denied");
                    self.sink.emit(
                        "cec://session",
                        json!({ "session_id": session_id, "state": "denied" }),
                    );
                }
                allmystuff_cec_protocol::ConnectControl::End { session_id } => {
                    self.cec.set_session(&session_id, "ended");
                    // The session an "Approve Once" covered is over — retire it
                    // now, so a later console open or re-dial has to earn a
                    // fresh approval instead of riding a leftover in-memory
                    // grant. Standing grants (3h / Forever) survive: outliving
                    // sessions is their whole point.
                    if self.cec.retire_once(&from) {
                        self.cec_emit_grants();
                    }
                    self.sink.emit(
                        "cec://session",
                        json!({ "session_id": session_id, "state": "ended" }),
                    );
                }
                allmystuff_cec_protocol::ConnectControl::Unknown => {}
            }
        }
    }

    /// Handle an inbound [`ChatMessage`](allmystuff_cec_protocol::ChatMessage)
    /// off the `cec.control` channel: attribute it to the **authenticated**
    /// sender (`from`) — never the message's self-declared `chat.from`, which is
    /// only the Role the far side renders as — append it to that peer's
    /// transcript, and surface it to the GUI. The sender's own line is echoed by
    /// [`Self::cec_chat_send`], so this path covers received lines only.
    async fn on_cec_chat_in(
        self: &Arc<Self>,
        from: &str,
        chat: &allmystuff_cec_protocol::ChatMessage,
    ) {
        let canonical = pubkey_part(from).to_string();
        tracing::info!(
            "cec chat in from {} ({} chars)",
            short_id(&canonical),
            chat.text.chars().count()
        );
        self.cec.push_chat(&canonical, chat.clone());
        self.emit_cec_chat(&canonical, chat);
    }

    /// Emit the `cec://chat` GUI event for one message on `peer`'s transcript —
    /// the single render path for both an inbound receive and the echo of a sent
    /// line, so the GUI has ONE way to draw a chat bubble. The message object is
    /// the wire [`ChatMessage`](allmystuff_cec_protocol::ChatMessage) serialized
    /// as-is (`from` → `"client"` / `"technician"`), so the event shape can never
    /// drift from the protocol type.
    fn emit_cec_chat(&self, peer: &str, chat: &allmystuff_cec_protocol::ChatMessage) {
        let message = serde_json::to_value(chat).unwrap_or(Value::Null);
        self.sink
            .emit("cec://chat", json!({ "peer": peer, "message": message }));
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
                Effect::StartMedia(route) => {
                    // The sender's transport pick inside start_media is
                    // one-shot too — same bring-up race guard, only for
                    // routes that carry a picture.
                    if matches!(route.media, MediaKind::Display | MediaKind::Video) {
                        self.await_video_bringup().await;
                        let still_active = self
                            .state
                            .lock()
                            .session
                            .as_ref()
                            .and_then(|s| s.route(&route.id))
                            .is_some_and(|live| {
                                live.state == RouteState::Active && live.route == route
                            });
                        if !still_active {
                            tracing::warn!(
                                "stale StartMedia for {} abandoned after video bring-up wait — route is no longer the active incarnation",
                                route.id
                            );
                            continue;
                        }
                        self.note_video_route_started(&route);
                    }
                    self.start_media(&route)
                }
                Effect::RefreshMedia(id) => self.video.force_idr(&id),
                Effect::TuneMedia {
                    route_id,
                    max_edge,
                    bitrate,
                    fps,
                    game,
                    mode,
                    ext: _, // pipeline-owned; no viewer-requested knob reads it yet
                } => self.video.retune_dials(
                    &route_id,
                    max_edge,
                    bitrate,
                    fps,
                    game,
                    mode.as_deref(),
                ),
                Effect::VideoFeedback {
                    route_id,
                    recv_fps,
                    decode_fails,
                    queue_depth,
                    lost_ts_us,
                    ext,
                } => {
                    // The pipeline's own feedback shape lives in the opaque
                    // ext — parse it here, at the backend edge, so the
                    // wire crates never learned what a bandwidth estimate
                    // is (the seam that keeps tuning backend-only).
                    let pf = crate::video::PipelineFeedback::from_ext(&ext);
                    if let Some(ts) = lost_ts_us {
                        // Frame health: the viewer named the AU that died.
                        // A GDR (game) route heals with an immediate
                        // refresh-wave restart — spread intra, no keyframe
                        // wall; everything else keeps the IDR refresh the
                        // feedback path already drives. (Targeted
                        // reference invalidation rides the ts-mapping
                        // follow-up.)
                        tracing::info!(
                            "frame health {route_id}: viewer lost AU at {ts} µs — targeted refresh"
                        );
                        self.video.route_wave_or_refresh(&route_id);
                    }
                    self.video.note_feedback(
                        &route_id,
                        recv_fps,
                        decode_fails,
                        queue_depth,
                        pf.est_kbps,
                        pf.delay_trend_us_per_s,
                    )
                }
                Effect::StopMedia(id) => {
                    // Effects are produced after the session mutates the route
                    // to TornDown/Rejected. If another task has already
                    // installed a same-id nonterminal successor, this stop
                    // belongs to the displaced incarnation and must not tear
                    // down the replacement by id. Offered/Incoming count too:
                    // early media can legitimately outrun Accept, and that is
                    // exactly the ABA window seen in the field logs.
                    let successor_is_live = self
                        .state
                        .lock()
                        .session
                        .as_ref()
                        .and_then(|s| s.route(&id))
                        .is_some_and(|r| {
                            matches!(
                                r.state,
                                RouteState::Offered | RouteState::Incoming | RouteState::Active
                            ) && matches!(r.route.media, MediaKind::Display | MediaKind::Video)
                        });
                    if successor_is_live {
                        tracing::warn!(
                            "stale StopMedia for {id} ignored because a same-id video successor is nonterminal"
                        );
                        continue;
                    }
                    let stop_state = self
                        .state
                        .lock()
                        .session
                        .as_ref()
                        .and_then(|s| s.route(&id))
                        .map(|r| format!("{:?}", r.state))
                        .unwrap_or_else(|| "absent".into());
                    tracing::info!(
                        "session StopMedia committing for {id} (route state {stop_state})"
                    );
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
                    self.drive_mounts.stop(&id);
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
        if let AppControl::KvmSupportAvailable { kvm, expires_in } = &message {
            // This announcement is meaningful only from a customer with whom
            // we have an active support session. It is not an authority claim:
            // the KVM separately receives and validates the customer's grant.
            if !self.cec.has_active_session_with(from.as_str()) {
                tracing::warn!(
                    "support-KVM announcement from {} ignored: no active customer session",
                    short_id(from.as_str())
                );
                return;
            }
            if !self
                .cec
                .note_support_kvm(from.as_str(), kvm.as_str(), *expires_in)
            {
                tracing::warn!(
                    "invalid support-KVM lease from {} ignored",
                    short_id(from.as_str())
                );
            }
            return;
        }
        // The ordinary AllMyStuff node never consumes a KVM grant. It is sent
        // to the appliance bridge, whose attached-computer check is the
        // authority boundary. Ignore it here without passing it through the
        // unrelated owner/fleet app-command gate.
        if matches!(&message, AppControl::KvmSupportGrant { .. }) {
            return;
        }
        if let AppControl::StageKvmMediaResult {
            request,
            complete,
            error,
        } = &message
        {
            if !*complete {
                let acknowledged = self
                    .kvm_media_requests
                    .lock()
                    .get_mut(request)
                    .filter(|pending| same_node(&pending.source, from.as_str()))
                    .and_then(|pending| pending.acknowledged.take());
                if let Some(acknowledged) = acknowledged {
                    let _ = acknowledged.send(());
                }
                return;
            }

            let pending = {
                let mut requests = self.kvm_media_requests.lock();
                let matches = requests
                    .get(request)
                    .is_some_and(|pending| same_node(&pending.source, from.as_str()));
                matches.then(|| requests.remove(request)).flatten()
            };
            if let Some(pending) = pending {
                self.sink.emit(
                    "allmystuff://kvm-media",
                    json!({
                        "from": from,
                        "kvm": pending.kvm,
                        "label": pending.label,
                        "error": error,
                    }),
                );
            }
            return;
        }

        let authorized = if matches!(
            &message,
            AppControl::MapDrive { .. } | AppControl::StageKvmMedia { .. }
        ) {
            self.sender_may_drive(from.as_str(), DrivePlane::Files)
        } else {
            self.sender_may_control(from.as_str())
        };
        if !authorized {
            tracing::warn!(
                "app-control {:?} from {} ignored: not owner/fleet",
                message,
                short_id(from.as_str())
            );
            return;
        }
        match message {
            AppControl::MapDrive {
                root,
                label,
                mount,
                request,
            } => {
                tracing::info!(
                    "native drive requested by {} from {}",
                    short_id(from.as_str()),
                    root
                );
                let mesh = self.clone();
                crate::spawn(async move {
                    if let Err(error) = mesh
                        .drive_map_requested(from.to_string(), root, label, mount, Some(request))
                        .await
                    {
                        tracing::warn!("native drive request failed: {error}");
                    }
                });
            }
            AppControl::StageKvmMedia {
                request,
                kvm,
                path,
                label,
            } => {
                tracing::info!(
                    "KVM virtual media requested by {} from {}",
                    short_id(from.as_str()),
                    path
                );
                if self
                    .send_control(
                        from.as_str(),
                        &ControlMessage::App(AppControl::StageKvmMediaResult {
                            request: request.clone(),
                            complete: false,
                            error: None,
                        }),
                    )
                    .await
                    .is_err()
                {
                    return;
                }
                let mesh = self.clone();
                let requester = from.to_string();
                crate::spawn(async move {
                    let error = mesh.kvm_media_stage(kvm, path, label).await.err();
                    if let Some(error) = &error {
                        tracing::warn!("KVM virtual media request failed: {error}");
                    }
                    let _ = mesh
                        .send_control(
                            &requester,
                            &ControlMessage::App(AppControl::StageKvmMediaResult {
                                request,
                                complete: true,
                                error,
                            }),
                        )
                        .await;
                });
            }
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
            AppControl::Restart => {
                tracing::info!(
                    "app restart requested by {} — relaunching this node",
                    short_id(from.as_str())
                );
                // No update, no network I/O — just relaunch onto the same
                // build (the OS-aware relaunch the sink owns). Like the upgrade
                // path, the confirmation is the node's next presence advert; no
                // reply is sent. Done on a fresh task so the relaunch's
                // never-returning exec/exit doesn't strand the inbound-frame
                // loop mid-handler.
                let sink = self.sink.clone();
                crate::spawn(async move {
                    sink.restart();
                });
            }
            AppControl::RestartDevice => {
                tracing::info!(
                    "device reboot requested by {} — handing to the OS",
                    short_id(from.as_str())
                );
                // Tell whoever is sitting at this machine why it's about to
                // go down, then ask the OS off the inbound-frame task. The
                // OS's own privilege rules still apply (see `crate::reboot`);
                // a refusal is logged rather than silently swallowed.
                self.sink.emit(
                    "allmystuff://device-restart",
                    serde_json::json!({ "from": from.as_str() }),
                );
                crate::spawn(async move {
                    match tokio::task::spawn_blocking(crate::reboot::restart_device).await {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => tracing::warn!("device reboot refused: {e}"),
                        Err(e) => tracing::warn!("device reboot task failed: {e}"),
                    }
                });
            }
            // An app command a newer build introduced that this one doesn't
            // implement (decoded as `Unknown` rather than failing the
            // control message) — nothing to act on.
            AppControl::KvmSupportGrant { .. }
            | AppControl::KvmSupportAvailable { .. }
            | AppControl::StageKvmMediaResult { .. }
            | AppControl::Unknown => {}
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
                    self.apply_fleet_release().await;
                }
            }
            OwnershipControl::Claimed { owner } => {
                // Honour a claim confirmation only from a device THIS node
                // actually sent a `Claim` to. Without this guard any
                // authenticated peer could send an *unsolicited* `Claimed` and
                // drive itself into our fleet member list *and* signed roster —
                // both of which `sender_may_control` trusts — i.e. hand itself
                // full control of this machine (input, shell, disk, clipboard).
                // This is the outbound-claim mirror of the per-sender guards the
                // sibling arms already apply (`Release`/`FleetKey` check the
                // recorded owner). Consumed on use, so a replayed or duplicate
                // confirmation is ignored.
                if !self
                    .pending_claims
                    .lock()
                    .remove(pubkey_part(from.as_str()))
                {
                    tracing::warn!(
                        "ignoring unsolicited claim confirmation from {} — this device never claimed it",
                        short_id(from.as_str())
                    );
                    return;
                }
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
                    // The handoff landed — the claim rendezvous has done its
                    // job. Tear the claim-code network down and rotate the
                    // (now spent) code.
                    self.ensure_claim_networks().await;
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

    /// The device-side fleet teardown — everything "this device just left
    /// / was let go from its fleet" implies, in one place: tear out of the
    /// fleet's closed network (purging its signed state so a later rejoin
    /// can't reload a stale genesis), clear the durable owner/key record,
    /// and re-broadcast the now-unowned presence. Shared by the
    /// cooperative path (the owner's `Release` frame) and the verified
    /// path (the daemon's `self_evicted` governance event — the device
    /// PROVED its own eviction from the signed log, which is stronger
    /// authority than any frame a peer could send). Deliberately does NOT
    /// re-enter claim mode: adoption is per-event consent on this device.
    async fn apply_fleet_release(self: &Arc<Self>) {
        // Tear out of the fleet's closed network before clearing the
        // credential (set_owner(None) drops the key it derives from).
        let fleet_net = self.ownership.fleet_network_id();
        self.ownership.set_owner(None);
        if let Some(network) = fleet_net {
            let _ = self
                .client
                // We've left this fleet — purge its signed state too: no
                // stale genesis to reload if we later join a different one.
                .request(&Request::NetworkRemove {
                    network,
                    purge: true,
                })
                .await;
        }
        self.refresh_fleet_authorization().await;
        self.ownership_check(None).await;
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

    /// The display label to hand a KVM so it can name itself `KVM-<target>` —
    /// the target's *real* advertised name, or empty when we don't know it.
    /// Distinct from [`Self::peer_label`], which falls back to a truncated id:
    /// here an unknown target must yield "" so the KVM falls back to the node
    /// id itself rather than being named `KVM-abcd1234ef…` with a literal
    /// ellipsis. The attach picker's default target is often *this* machine,
    /// which is never a session peer, so that case is resolved from our own
    /// presence profile.
    fn attach_target_label(&self, target: &NodeId) -> String {
        let canon = pubkey_part(target.as_str());
        let st = self.state.lock();
        // This machine (the picker's frequent default): our own profile label.
        if let Some(p) = st.profile.as_ref() {
            if target.is_this() || pubkey_part(p.node.as_str()) == canon {
                let l = p.label.trim();
                return if l.is_empty() {
                    String::new()
                } else {
                    l.to_string()
                };
            }
        }
        if let Some(session) = st.session.as_ref() {
            for p in session.peers() {
                if pubkey_part(p.node.as_str()) == canon && !p.label.trim().is_empty() {
                    return p.label.clone();
                }
            }
        }
        String::new()
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
                // The device-local public-claims setting rides the owned
                // payload in both shapes — the toggle is usable before a
                // fleet exists (it gates this machine's own claiming).
                o.insert(
                    "public_claims".into(),
                    Value::Bool(self.ownership.public_claims()),
                );
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
                                    // A projection of the signed roster for the
                                    // GUI, not this owner's local record — the
                                    // claim stamp lives on the latter and is
                                    // read only by the eviction prune.
                                    claimed_at: None,
                                });
                            }
                        }
                    }
                }
            }
        }
        // Member-side resilience for the signed roster (the symmetric twin of
        // the owner's `fleet_members()` fallback below). `members` here holds
        // exactly what the closed network's signed roster returned. A non-empty
        // read is authoritative — cache it. An empty read means the fleet's
        // closed network is momentarily unreadable (mid-(re)join), not that the
        // fleet emptied: fall back to the last cached roster so a co-member
        // doesn't flicker to "another fleet" during a reconnect. Because a
        // non-empty read always replaces the cache, an eviction propagates the
        // instant the roster is readable again.
        if members.is_empty() {
            for m in self.fleet_roster_cache.lock().iter() {
                let canon = pubkey_part(m.device.as_str()).to_string();
                member_roles
                    .entry(canon)
                    .or_insert_with(|| "member".to_string());
                members.push(m.clone());
            }
        } else {
            *self.fleet_roster_cache.lock() = members.clone();
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
                    claimed_at: m.claimed_at,
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
                    claimed_at: None,
                });
            }
            // Best-effort role for this device (it isn't in its own roster):
            // the founder is the owner, everyone else defaults to member.
            member_roles.entry(canon).or_insert_with(|| {
                if self.ownership.is_fleet_owner() {
                    "owner"
                } else {
                    "member"
                }
                .to_string()
            });
        }
        // The whole fleet should see *who the owner is*, not just the owner
        // machine. A member always knows its owner locally — the device that
        // claimed it — so stamp that device "owner" here, covering the window
        // before the closed network's signed roster converges its role and the
        // owned-but-keyless case (claimed, no closed network yet, so no roster
        // to read at all). `or_insert` never overrides a role the signed roster
        // already projected, and the owner is added to the member list if the
        // roster hasn't surfaced it yet (label left blank — the GUI resolves it
        // by canonical id). The MyOwnMesh roster gossip converges the same fact
        // network-wide; this is the local fast path / fallback.
        if !self.ownership.is_fleet_owner() {
            if let Some(owner) = self.ownership.owner() {
                let canon = pubkey_part(&owner).to_string();
                if !members
                    .iter()
                    .any(|m| pubkey_part(m.device.as_str()) == canon)
                {
                    members.push(OwnedMember {
                        device: NodeId::from(canon.as_str()),
                        label: String::new(),
                        claimed_at: None,
                    });
                }
                member_roles
                    .entry(canon)
                    .or_insert_with(|| "owner".to_string());
            }
        }
        let roster = OwnedRoster {
            key,
            name: self.ownership.fleet_name(),
            version: self.ownership.fleet_version(),
            members,
        };
        // "Owner" for the GUI is the **signed** owner role OR the structural
        // key-holder — a device the founder granted the owner role is a full
        // owner and must see owner actions (evict, promote, …), not be gated out
        // as a second-class member.
        let is_owner_flag = self.ownership.is_fleet_owner()
            || match network.as_deref() {
                Some(n) => self.is_fleet_owner_signed(n).await,
                None => false,
            };
        let governed_topology = match network.as_deref() {
            Some(n) => self.fleet_governed_topology(n).await.unwrap_or(Value::Null),
            None => Value::Null,
        };
        let mut value = serde_json::to_value(roster).unwrap_or_else(|_| empty_owned());
        if let Some(obj) = value.as_object_mut() {
            // Whether this device may take owner actions (signed owner or the
            // structural key-holder), so the GUI can gate owner-only controls.
            obj.insert("is_owner".into(), Value::Bool(is_owner_flag));
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
            // This device's public-claims setting (device-local, never
            // synced) — the Fleet pane's toggle reads it from here.
            obj.insert(
                "public_claims".into(),
                Value::Bool(self.ownership.public_claims()),
            );
            // Governed topology (daemon ≥ 0.2.36): the owner-signed
            // network-wide shape the fleet runs, or null when ungoverned
            // (or the daemon predates it). The Fleet pane's infra-hub
            // toggles render from this.
            obj.insert("topology".into(), governed_topology);
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
        // Claimer-side mirror of the claimee's arrival-network gate: with
        // public claims off (the default), a claim only goes out over the
        // LAN claim rendezvous. The error names the fix — either walk the
        // two machines onto one local network, or deliberately enable the
        // public-mesh path on this device.
        if !self.ownership.public_claims_allowed() {
            let route = self.network_for_peer(&node);
            if route.as_deref() != Some(LOCAL_CLAIM_NETWORK_ID) {
                return Err(
                    "this device was discovered over a public mesh — put both machines on \
                     the same local network, or enable \"Allow claiming over the public \
                     mesh\" in Fleet settings on this machine"
                        .into(),
                );
            }
        }
        // Record that we're now awaiting this device's `Claimed` confirmation,
        // so the inbound handler honours only a confirmation we actually
        // solicited (see `pending_claims` / the `Claimed` arm). Recorded before
        // the send; if the send fails the peer never answers, so the leftover
        // entry is harmless.
        self.pending_claims
            .lock()
            .insert(pubkey_part(&node).to_string());
        tracing::info!("claiming {} (sending ownership claim)", short_id(&node));
        let msg = ControlMessage::Ownership(OwnershipControl::Claim { owner: me.into() });
        self.send_control(&node, &msg).await
    }

    /// Front-end command: claim a **remote** device by the claim code its
    /// operator read off it (device web UI, service log). Joins the code's
    /// randomized rendezvous network — unguessable, so unlike the old
    /// well-known public claim mesh nobody can lurk there — waits for the
    /// device's claimable presence, sends the claim, waits for it to land
    /// in the fleet, and tears the rendezvous down again either way.
    pub async fn claim_via_code(self: &Arc<Self>, code: String) -> Result<(), String> {
        if !self.ownership.public_claims_allowed() {
            return Err(
                "remote claiming is off on this device — enable \"Allow claiming over the \
                 public mesh\" in Fleet settings first"
                    .into(),
            );
        }
        let network = claim_code_network_id(&code);
        if network == claim_code_network_id("") {
            return Err("enter the claim code shown on the device".into());
        }
        tracing::info!("remote claim: joining rendezvous {network}");
        let _ = self
            .client
            .request(&Request::NetworkAdd {
                config: json!({
                    "id": network.as_str(),
                    "network_id": network.as_str(),
                    "label": "Remote claiming",
                    "kind": "open",
                    "auto_approve": true,
                    "signaling": { "strategy": "nostr", "mdns": true },
                }),
            })
            .await;
        self.sync_networks().await;

        let result = self.claim_via_code_inner(&network).await;

        // Rendezvous down again, success or not — it existed for this one
        // claim. `purge` drops its signed-state residue too.
        let _ = self
            .client
            .request(&Request::NetworkRemove {
                network: network.clone(),
                purge: true,
            })
            .await;
        self.sync_networks().await;
        result
    }

    async fn claim_via_code_inner(self: &Arc<Self>, network: &str) -> Result<(), String> {
        // Wait for the device's claimable presence on the rendezvous.
        const DISCOVER_DEADLINE: std::time::Duration = std::time::Duration::from_secs(75);
        const CLAIM_DEADLINE: std::time::Duration = std::time::Duration::from_secs(45);
        const POLL: std::time::Duration = std::time::Duration::from_millis(500);

        let discover_by = std::time::Instant::now() + DISCOVER_DEADLINE;
        let target = loop {
            if let Some(node) = self.claimable_on_network(network) {
                break node;
            }
            if std::time::Instant::now() > discover_by {
                return Err(
                    "no claimable device answered on that code — check the code, make sure \
                     remote claiming is still enabled on the device, and that it is online"
                        .into(),
                );
            }
            tokio::time::sleep(POLL).await;
        };

        tracing::info!(
            "remote claim: found claimable device {} on the rendezvous",
            short_id(&target)
        );
        self.claim(target.clone()).await?;

        // The `Claimed` reply mints the fleet key and records the member —
        // that's the durable signal the claim landed.
        let claimed_by = std::time::Instant::now() + CLAIM_DEADLINE;
        loop {
            let claimed = self
                .ownership
                .fleet_member_ids()
                .iter()
                .any(|m| pubkey_part(m) == pubkey_part(&target));
            if claimed {
                return Ok(());
            }
            if std::time::Instant::now() > claimed_by {
                return Err(
                    "the device saw the claim but confirmation never arrived — it may have \
                     declined (already owned, or claim mode off); check its screen or logs"
                        .into(),
                );
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// A claimable node whose last-seen network is `network`, if any.
    fn claimable_on_network(&self, network: &str) -> Option<String> {
        let st = self.state.lock();
        let session = st.session.as_ref()?;
        let mut claimables = session
            .peers()
            .filter(|p| p.claimable)
            .map(|p| p.node.to_string());
        claimables.find(|id| {
            st.peer_networks
                .get(pubkey_part(id))
                .is_some_and(|net| net == network)
        })
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

    /// Ask a fleet machine to **restart** its AllMyStuff app (relaunch onto the
    /// same build — no update). The target enforces owner/fleet before acting;
    /// its next presence advert is the confirmation.
    pub async fn request_restart(self: &Arc<Self>, node: String) -> Result<(), String> {
        tracing::info!("asking {} to restart its app", short_id(&node));
        let msg = ControlMessage::App(AppControl::Restart);
        self.send_control(&node, &msg).await
    }

    /// Front-end command: reboot a machine's whole OS — the recovery step
    /// heavier than [`Mesh::request_restart`]. Our own device reboots
    /// directly (no wire round-trip to ourselves); a peer is asked with
    /// [`AppControl::RestartDevice`], gated owner/fleet on its side exactly
    /// like the app restart. Its presence dropping and returning is the
    /// confirmation. An older peer decodes the command as `Unknown` and
    /// ignores it — the ask goes unanswered, never misread.
    pub async fn request_restart_device(self: &Arc<Self>, node: String) -> Result<(), String> {
        let is_self = self
            .local_node_id()
            .is_some_and(|me| pubkey_part(&node) == pubkey_part(&me));
        if is_self {
            tracing::info!("rebooting this device (asked from its own gear menu)");
            return tokio::task::spawn_blocking(crate::reboot::restart_device)
                .await
                .map_err(|e| e.to_string())?;
        }
        tracing::info!("asking {} to reboot its device", short_id(&node));
        let msg = ControlMessage::App(AppControl::RestartDevice);
        self.send_control(&node, &msg).await
    }

    /// Re-learn a node's details on demand — the per-node refresh control.
    ///
    /// For **this** device (`None`, or our own id), re-scan its hardware and
    /// re-advertise the fresh profile, so both our own capabilities and what
    /// peers see of us update. For a **peer**, re-stamp + re-send our presence
    /// to it (an ownership/fleet re-sync that also nudges it) and re-request its
    /// exposed sites; the daemon already holds the peer's latest capability
    /// advert, so the GUI's follow-up resync picks up the rest. Best-effort: a
    /// site request to a non-managed peer is simply refused on the far side.
    pub async fn refresh_node(self: &Arc<Self>, node: Option<String>) -> Result<(), String> {
        let is_self = match (&node, self.local_node_id()) {
            (None, _) => true,
            (Some(n), Some(me)) => pubkey_part(n) == pubkey_part(&me),
            _ => false,
        };
        if is_self {
            tracing::info!("refresh: re-scanning this device + re-advertising");
            self.restamp_profile().await;
            return Ok(());
        }
        let peer = node.unwrap_or_default();
        if peer.is_empty() {
            return Ok(());
        }
        // One backoff tick guards every peer-bound action of a refresh, so a
        // held-down refresh can't hammer the peer (the envelope grows from once
        // every 5 s to once a minute over a sustained burst).
        if !self.allow_profile_request(&peer) {
            tracing::debug!("refresh of {} throttled by backoff", short_id(&peer));
            return Ok(());
        }
        tracing::info!("refresh: re-learning {}", short_id(&peer));
        // The guaranteed round-trip: ask the peer to re-announce its profile so
        // we re-learn it now (it answers with an ordinary presence advert).
        let _ = self
            .send_control(&peer, &ControlMessage::ProfileRequest)
            .await;
        // And re-sync our ownership/fleet view + its exposed sites while we're
        // here.
        self.ownership_check(Some(pubkey_part(&peer))).await;
        let _ = self.site_remote_list(peer).await;
        Ok(())
    }

    /// Reconnect mesh transport **in place** — redial signaling and renegotiate
    /// ICE without leaving the room. The non-destructive twin of a leave+rejoin
    /// (`network_set_enabled` off-then-on): every session and all app-level
    /// state survives, so a refresh on one side never strands the other.
    ///
    /// Resolution of what to reconnect: a set `network` is every peer on that
    /// mesh (the global refresh control); `peer` alone is that one node, on the
    /// mesh it's reachable on (the same network resolution our sends use, for
    /// the per-node refresh); neither is every joined mesh.
    ///
    /// Best-effort: a per-network failure is logged and the rest still run.
    pub async fn reconnect(
        self: &Arc<Self>,
        network: Option<String>,
        peer: Option<String>,
    ) -> Result<(), String> {
        let networks: Vec<String> = match (&network, &peer) {
            (Some(net), _) => vec![net.clone()],
            (None, Some(p)) => self.network_for_peer(p).into_iter().collect(),
            (None, None) => self.state.lock().networks.clone(),
        };
        if networks.is_empty() {
            return Err("no joined network to reconnect on".into());
        }
        // The daemon keys peer sessions by canonical pubkey, so strip any
        // display decoration off the node id before forwarding.
        let peer_canon = peer.as_deref().map(|p| pubkey_part(p).to_string());
        let mut any_ok = false;
        let mut last_err: Option<String> = None;
        for net in networks {
            match self
                .client
                .request(&Request::NetworkReconnect {
                    network: net.clone(),
                    peer: peer_canon.clone(),
                })
                .await
            {
                Ok(resp) if resp.ok => any_ok = true,
                Ok(resp) => {
                    let e = resp.error.unwrap_or_else(|| "reconnect rejected".into());
                    tracing::warn!("reconnect on {net}: {e}");
                    last_err = Some(e);
                }
                Err(e) => {
                    tracing::warn!("reconnect on {net} failed: {e}");
                    last_err = Some(e.to_string());
                }
            }
        }
        // A partial success still counts as success (the failed mesh is logged
        // above); a total failure surfaces so the GUI can report it.
        if any_ok {
            Ok(())
        } else {
            Err(last_err.unwrap_or_else(|| "reconnect failed".into()))
        }
    }

    /// Whether a refresh round-trip to `peer` is allowed under the backoff
    /// envelope right now, recording it as sent when it is. Keyed per canonical
    /// peer so refreshing different machines stays independent. See
    /// [`profile_req_decide`] for the envelope itself.
    fn allow_profile_request(&self, peer: &str) -> bool {
        let now = std::time::Instant::now();
        let key = pubkey_part(peer).to_string();
        let mut map = self.profile_req.lock();
        let (allow, st) = profile_req_decide(map.get(&key).copied(), now);
        map.insert(key, st);
        allow
    }

    /// Front-end command: point a KVM appliance (`node`) at the machine it
    /// controls (`target`). The KVM enforces owner/fleet before applying, then
    /// re-advertises its new binding ([`NodeProfile::kvm`]) — that presence is
    /// the authoritative confirmation, exactly as a claim confirms by
    /// re-advertising its new owner. A send the daemon couldn't deliver is
    /// surfaced so the UI can say so rather than leaving the ask hanging.
    pub async fn kvm_attach(self: &Arc<Self>, node: String, target: String) -> Result<(), String> {
        tracing::info!("pointing KVM {} at {}", short_id(&node), short_id(&target));
        // Ride the target's display label along so the KVM can rename itself
        // `KVM-<label>` — best-effort and cosmetic (empty when the target has
        // no label we know; the KVM then falls back to the node id, never a
        // truncated-id string).
        let label = self.attach_target_label(&NodeId::from(target.clone()));
        let msg = ControlMessage::Kvm(KvmControl::Attach {
            node: target.into(),
            label,
        });
        self.send_control(&node, &msg).await
    }

    /// Front-end command: clear a KVM appliance's binding — it no longer
    /// represents any machine. Same delivery + presence-confirmation model as
    /// [`Mesh::kvm_attach`].
    pub async fn kvm_detach(self: &Arc<Self>, node: String) -> Result<(), String> {
        tracing::info!("detaching KVM {}", short_id(&node));
        let msg = ControlMessage::Kvm(KvmControl::Detach);
        self.send_control(&node, &msg).await
    }

    /// Front-end command: walk a KVM appliance onto another mesh — the fleet
    /// owner's membership tool. The KVM validates the id, refuses its own
    /// fleet mesh, joins, and re-advertises [`NodeProfile::kvm`] with the new
    /// membership list — that presence is the authoritative confirmation.
    pub async fn kvm_mesh_add(
        self: &Arc<Self>,
        node: String,
        network_id: String,
    ) -> Result<(), String> {
        let network_id = network_id.trim().to_lowercase();
        if network_id.is_empty() {
            return Err("a mesh name is required".into());
        }
        tracing::info!("asking KVM {} to join mesh {network_id}", short_id(&node));
        let msg = ControlMessage::Kvm(KvmControl::MeshAdd { network_id });
        self.send_control(&node, &msg).await
    }

    /// Front-end command: take a KVM appliance off a mesh. The KVM refuses
    /// its fleet mesh (that membership is governed by the fleet key); same
    /// presence-confirmation model as [`Mesh::kvm_mesh_add`].
    pub async fn kvm_mesh_remove(
        self: &Arc<Self>,
        node: String,
        network_id: String,
    ) -> Result<(), String> {
        let network_id = network_id.trim().to_lowercase();
        if network_id.is_empty() {
            return Err("a mesh name is required".into());
        }
        tracing::info!("asking KVM {} to leave mesh {network_id}", short_id(&node));
        let msg = ControlMessage::Kvm(KvmControl::MeshRemove { network_id });
        self.send_control(&node, &msg).await
    }

    /// Front-end command: put *this* device into (or out of) claim mode, so
    /// another of your machines can adopt it. Re-advertises immediately.
    pub async fn set_claimable(self: &Arc<Self>, on: bool) -> Result<bool, String> {
        self.ownership.set_claim_mode(on);
        // Claim-rendezvous membership follows claim mode (the claim-code
        // network only exists while claimable with public claims on).
        self.ensure_claim_networks().await;
        self.refresh_profile_ownership().await;
        Ok(self.ownership.claimable())
    }

    /// Front-end command: flip **this device's** public-claims setting —
    /// whether it participates in claiming over the public mesh, in either
    /// role (offering itself via a claim code while claimable; claiming
    /// remote devices by code as an owner). Strictly device-local: it is
    /// never synced from a fleet and no remote peer can flip it. Off by
    /// default; claiming stays LAN-only until someone at this machine turns
    /// it on.
    pub async fn set_public_claims(self: &Arc<Self>, on: bool) -> Result<bool, String> {
        if !self.ownership.set_public_claims(on) {
            return Err("couldn't persist the setting".into());
        }
        tracing::info!(
            "claims over the public mesh {} on this device",
            if on { "ENABLED" } else { "disabled" }
        );
        // Rendezvous membership and presence both follow the setting.
        self.ensure_claim_networks().await;
        self.refresh_profile_ownership().await;
        self.emit_owned().await;
        Ok(self.ownership.public_claims())
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
                // Re-stamp the fleet metadata too: a claim/adopt/leave/rename
                // is exactly when the fleet name + owner change, and this is the
                // path that re-broadcasts presence, so peers regroup correctly.
                p.fleet_name = self.ownership.fleet_name();
                p.fleet_owner = self.fleet_owner_name(&p.label.clone());
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
            // Claim rendezvous follows claim state on the same cadence: the
            // LAN claim network is (re)asserted, and the claim-code network
            // comes up / goes down / rotates with claimability.
            self.ensure_claim_networks().await;
            // Same cadence, opposite policy: every *non*-fleet mesh is made
            // fully open (auto-approve), so older meshes are migrated and any
            // newly joined one is reconciled — no mesh keeps a stale approval
            // gate now that the approval queue is gone.
            self.ensure_open_meshes_auto_approve().await;
        }
        self.refresh_fleet_authorization().await;
    }

    /// Keep the claim-rendezvous networks in step with this device's claim
    /// state. Two networks, two scopes:
    ///
    ///  * the **local claim network** ([`LOCAL_CLAIM_NETWORK_ID`]) — always
    ///    joined, LAN-only (daemon signaling `strategy:"none", mdns:true`,
    ///    no STUN/TURN). Claimable presence lives here; a claimer discovers
    ///    a claimable box here with zero configuration and zero public
    ///    infrastructure. This is the default — and with public claims off,
    ///    the only — claim path.
    ///  * the **claim-code network** (`amsclaim-<code>`) — the WAN
    ///    rendezvous, joined only while this device sits claimable *and*
    ///    public claims are deliberately enabled on it (the device-local
    ///    setting or `ALLMYSTUFF_PUBLIC_CLAIMS`). The code is unguessable
    ///    and shown out-of-band (log line here; a device UI elsewhere), so
    ///    strangers can't find — let alone race-claim — the box the way
    ///    they could on a well-known open network. Kept joined through the
    ///    claimed-but-keyless window so the `Claimed` reply and the
    ///    `FleetKey` handoff can still ride it, then torn down, with the
    ///    code rotated once the fleet key lands (a code that admitted an
    ///    owner is spent).
    async fn ensure_claim_networks(self: &Arc<Self>) {
        // The always-on LAN rendezvous. Explicit empty STUN/TURN lists opt
        // out of the daemon's public defaults — this network must touch no
        // remote infrastructure at all. A duplicate NetworkAdd (already
        // joined) returns an error we ignore, same as the fleet network.
        // "Always-on" bows to one thing: the user flipping it *off* (the
        // network can't be left, only toggled, so the park store is its
        // only off switch) — re-joining here would make the toggle snap
        // back on at the next claim-state change.
        if !self.network_parked(LOCAL_CLAIM_NETWORK_ID) {
            let _ = self
                .client
                .request(&Request::NetworkAdd {
                    config: json!({
                        "id": LOCAL_CLAIM_NETWORK_ID,
                        "network_id": LOCAL_CLAIM_NETWORK_ID,
                        "label": "Local (this LAN)",
                        "kind": "open",
                        "auto_approve": true,
                        "signaling": { "strategy": "none", "mdns": true },
                        "stun_servers": [],
                        "turn_servers": [],
                    }),
                })
                .await;
        }

        // The WAN rendezvous, tracking claim state.
        let claimable = self.ownership.claimable();
        let public_ok = self.ownership.public_claims_allowed();
        let keyless_claimed =
            self.ownership.owner().is_some() && self.ownership.fleet_key().is_none();
        if claimable && public_ok {
            let code = self.ownership.ensure_claim_code();
            let network = claim_code_network_id(&code);
            tracing::info!(
                "remote claiming enabled — claim code: {}  (claim this device from \
                 another machine's Fleet settings by entering that code)",
                format_claim_code(&code)
            );
            let _ = self
                .client
                .request(&Request::NetworkAdd {
                    config: json!({
                        "id": network.as_str(),
                        "network_id": network.as_str(),
                        "label": "Remote claiming",
                        "kind": "open",
                        "auto_approve": true,
                        "signaling": { "strategy": "nostr", "mdns": true },
                    }),
                })
                .await;
        } else if let Some(code) = self.ownership.claim_code() {
            if !keyless_claimed {
                // Not claimable and not waiting on a fleet-key handoff —
                // the rendezvous has no business staying up.
                let _ = self
                    .client
                    .request(&Request::NetworkRemove {
                        network: claim_code_network_id(&code),
                        purge: false,
                    })
                    .await;
                if self.ownership.owner().is_some() {
                    // Fully claimed (owner + fleet key): this code admitted
                    // an owner and is spent.
                    self.ownership.rotate_claim_code();
                }
            }
        }
        self.sync_networks().await;
    }

    /// Whether an inbound `Claim` arriving on `network` may be honored.
    /// LAN-first policy: the local claim network always may; anything else
    /// (the claim-code rendezvous, a shared public mesh from a legacy
    /// claimer) only when public claims are deliberately enabled **on this
    /// device**.
    fn claim_network_allowed(&self, network: &str) -> bool {
        network == LOCAL_CLAIM_NETWORK_ID || self.ownership.public_claims_allowed()
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

        // "Owner" for admit purposes is the **signed** role, not just the
        // structural key-holder: a device the founder granted the owner role is
        // a full owner and admits members like any other. (Founding itself is
        // still gated on `is_fleet_founder` below — only the key-minter elects
        // the genesis — but every owner runs the admit loop.)
        let is_owner =
            self.ownership.is_fleet_owner() || self.is_fleet_owner_signed(&network).await;
        // A **manager** (controller) isn't an owner but the signed governance
        // gives it authority to admit members too, so it also runs the admit
        // loop. We read the *converged* signed role, so a freshly-promoted
        // manager/owner only starts admitting once it has adopted the grant.
        let is_manager = !is_owner && self.is_fleet_manager(&network).await;
        if !is_owner {
            // A non-owner pre-rosters its **owner**. Fleet membership is mutual
            // trust established by the claim, but MyOwnMesh only auto-approves a
            // connection from a peer that's already in your roster — so without
            // this the device would be prompted to "let in" its own owner (and
            // approving it would admit the owner via the handshake). The owner
            // already pre-rosters the member at claim time; this is the
            // symmetric half. We trust our owner inherently (it owns us), so
            // there's no authority gap.
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
            // A plain member has no roster authority and stops here; a manager
            // continues to the admit loop.
            if !is_manager {
                return;
            }
        }

        // Custody lock: if this device enrolled a per-network TOTP, the daemon
        // requires a fresh code to *author* any governance transition — which
        // this background loop can't supply. Firing silent `mfa_code: None`
        // founds/admits would just be refused on every startup (and, pre-fix,
        // looked like "the fleet roster silently stopped updating"). So when
        // locked, skip the signed-governance steps here and let the owner author
        // founding + admits interactively from the Governance UI (with a code) —
        // which is the whole point of the lock. The local `RosterApprove` calls
        // below are NOT custody-gated (they're roster ops, not governance
        // authoring), so peer auto-approve still reflects members either way.
        let custody_locked = self.fleet_mfa_enrolled(&network).await;
        if custody_locked {
            tracing::info!(
                "fleet network {network} is custody-locked — skipping automatic \
                 found/admit; author membership changes from the Governance UI"
            );
        }

        // Found the closed governance only if we're the genuine **founder** —
        // the device that MINTED the fleet key. A structural owner that merely
        // adopted a key must NOT self-elect a parallel genesis: the engine would
        // (correctly) refuse to merge it, leaving two split-brain fleets that
        // only a deliberate leave-and-rejoin can consolidate. A manager never
        // founds either. `is_fleet_founded` reads the signed state.
        if !custody_locked
            && self.ownership.is_fleet_founder()
            && !self.is_fleet_founded(&network).await
        {
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

        // Before the admit loop re-asserts our claimed-list, reconcile it against
        // the signed governance. A device *another* owner evicted converges out of
        // the signed roster network-wide, but that eviction never reached THIS
        // owner's local member list — it's only pruned locally by the owner who
        // authored the kick (`kick_member`). Left in place, the loop below would
        // re-sign the device into the member log (a fresh, later-stamped admit that
        // wins the last-writer-wins tie) and re-approve it — silently resurrecting
        // an evicted device across the whole fleet, so co-owners "can't see it
        // gone." Drop any locally listed device the signed logs have removed so the
        // eviction sticks here too. Best-effort: an empty set (no fleet, an older
        // daemon that doesn't report it, or a read error) prunes nothing, so a
        // transient failure never drops a live member.
        let signed_evicted = self.signed_evicted(&network).await;
        if !signed_evicted.is_empty() {
            let mut pruned = false;
            for m in self.ownership.fleet_members() {
                let member = m.device.to_string();
                let Some(&evicted_at) = signed_evicted.get(pubkey_part(&member)) else {
                    continue;
                };
                // Compare stamps rather than mere set membership. Membership
                // converges last-writer-wins, so an eviction only outranks a
                // claim that came before it. A device this owner deliberately
                // re-claimed AFTER the eviction is the later intent, and the
                // admit loop below authors the superseding grant the governance
                // layer already honours (`re_admitting_an_evicted_member_
                // supersedes_the_tombstone`). Pruning on set membership alone
                // dropped it here first, every time, so that grant was never
                // written and unclaim → re-claim was a one-way door.
                //
                // An unstamped (pre-upgrade) record sorts as older, so it keeps
                // the previous always-prune behaviour; re-claiming stamps it.
                if m.claimed_at.is_some_and(|at| at > evicted_at) {
                    tracing::info!(
                        "keeping {} — re-claimed after the signed eviction; re-admitting it",
                        short_id(&member)
                    );
                    continue;
                }
                tracing::info!(
                    "pruning {} from the local fleet list — the signed governance evicted it",
                    short_id(&member)
                );
                let _ = self.ownership.kick_member(&member);
                pruned = true;
            }
            if pruned {
                // Reflect the removal now: the authorised-controller cache and the
                // GUI's fleet roster must both drop the evicted device immediately,
                // not on the next poll.
                self.refresh_fleet_authorization().await;
                self.emit_owned().await;
            }
        }

        // Admit every fleet member by **signing** them into the closed
        // network's **member log** — a ratified `RoleGrant` authored by an owner
        // or manager. This is what makes membership signed and self-sufficient:
        // every other member derives the complete roster from the *verified*
        // log, so they no longer depend on receiving live (unsigned) roster
        // gossip while the author happens to be online. That dependency was the
        // fleet bug — a member couldn't see its co-members until the owner
        // re-gossiped. The member log is union-merged, so a manager re-asserting
        // here converges with the owner's admits instead of forking them.
        //
        // We sign in only members the log doesn't already carry. Re-granting
        // `member` to someone already signed would be a redundant transition at
        // best, and — for a device we'd promoted to manager/owner — a *demotion*
        // back to member. So pull who's already signed (any role) and skip them;
        // this also migrates fleets whose members were only ever plain
        // roster-approved before signed membership (they aren't in the log yet,
        // so they get signed now). Re-asserting on every startup is therefore
        // free once converged. We keep the local `RosterApprove` for everyone so
        // our own auto-approve and peer list reflect each member immediately,
        // before ratification mirrors the grant into the roster projection.
        let already_signed = self.signed_role_holders(&network).await;
        let me = self.local_node_id().map(|m| pubkey_part(&m).to_string());
        for member in self.ownership.fleet_member_ids() {
            let device_id = pubkey_part(&member).to_string();
            // Never author a grant over ourselves: the founder election already
            // made us Owner, and a `member` grant here would demote us.
            if Some(&device_id) == me.as_ref() {
                continue;
            }
            if !custody_locked && !already_signed.contains(&device_id) {
                let _ = self
                    .client
                    .request(&Request::GovernanceProposeRoleGrant {
                        network: network.clone(),
                        target: device_id.clone(),
                        role: "member".to_string(),
                        mfa_code: None,
                    })
                    .await;
            }
            let _ = self
                .client
                .request(&Request::RosterApprove {
                    network: network.clone(),
                    device_id,
                    label: None,
                })
                .await;
        }
    }

    /// Make every ordinary (non-fleet) mesh fully open by turning on
    /// `auto_approve`: any node that joins is admitted automatically, with no
    /// per-mesh approval gate. AllMyStuff shapes who can mesh with you through
    /// private venues, the Fleet, and Sharing — not by approving devices one by
    /// one — so the approval queue is gone and every mesh must auto-admit or
    /// peers would be stranded with no way in.
    ///
    /// New meshes are already created auto-approve by the GUI; this migrates any
    /// older mesh (made before the open default, or joined some other way) on
    /// the next launch. The fleet's own mesh is **skipped**: its membership is
    /// the signed roster (claim-based), never open admission, so a stranger can
    /// never auto-join it. Idempotent — a mesh already open is left untouched,
    /// so there is no churn after the first pass.
    async fn ensure_open_meshes_auto_approve(self: &Arc<Self>) {
        let fleet = self.ownership.fleet_network_id();
        let resp = match self.client.request(&Request::ConfigShow).await {
            Ok(r) if r.ok => r,
            _ => return,
        };
        let Some(data) = resp.data else { return };
        let Some(nets) = data.pointer("/config/networks").and_then(|v| v.as_array()) else {
            return;
        };
        // Snapshot the configs that need flipping first, so no borrow of `data`
        // is held across the awaited NetworkUpdate calls below.
        let to_open: Vec<Value> = nets
            .iter()
            .filter(|n| {
                let nid = n
                    .get("network_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                if nid.is_empty() {
                    return false;
                }
                // CEC owns its admission policy. The public area and queue are
                // signaling-only; selected customer rooms opt into admission
                // explicitly. Never auto-open any of them here.
                if crate::cec::is_cec_network(nid) {
                    return false;
                }
                // Never auto-open the fleet's closed mesh — its members are the
                // signed roster, not anyone who connects.
                if fleet.as_deref() == Some(nid) {
                    return false;
                }
                // Already open → nothing to do (keeps this idempotent).
                !n.get("auto_approve")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        for mut config in to_open {
            let nid = config
                .get("network_id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            // Safety net for closed networks. The fleet-id match above is the
            // primary skip, but if `fleet_network_id` is momentarily unset
            // (mid-leave, or the ownership store still loading) the fleet's mesh
            // could slip past it — and auto-opening a *closed* governance network
            // would let anyone connect straight into it. So never open a network
            // whose **signed** governance is closed, whatever its config `kind`
            // (the fleet mesh is created `open` then transitioned, so the config
            // field lies) or our fleet state says.
            if self.is_closed_governance(&nid).await {
                tracing::debug!(
                    "leaving closed-governance mesh {nid} approval-gated (not auto-opened)"
                );
                continue;
            }
            if let Some(obj) = config.as_object_mut() {
                obj.insert("auto_approve".into(), Value::Bool(true));
            }
            // A full-config round-trip (only `auto_approve` changed) — the same
            // shape `network_set_enabled` parks and re-adds, so the daemon
            // hot-applies it without dropping live peers.
            match self
                .client
                .request(&Request::NetworkUpdate { config })
                .await
            {
                Ok(r) if r.ok => tracing::info!("opened mesh {nid} — auto-approve on (fully open)"),
                Ok(r) => tracing::warn!(
                    "couldn't open mesh {nid}: {}",
                    r.error.unwrap_or_else(|| "(no error)".into())
                ),
                Err(e) => tracing::warn!("couldn't open mesh {nid}: {e}"),
            }
        }
    }

    /// Whether `network`'s **authoritative** governance — the signed state log,
    /// not the config's initial `kind` field — is closed. A closed network must
    /// never be auto-opened: its membership is the signed roster, not anyone who
    /// connects. Mirrors the GovernanceState read in [`Mesh::is_fleet_founded`].
    /// Any error reads as *not* closed: the fleet-id skip in
    /// [`Mesh::ensure_open_meshes_auto_approve`] remains the first line of
    /// defence, and an ordinary open mesh has no governance log to consult.
    async fn is_closed_governance(self: &Arc<Self>, network: &str) -> bool {
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
        data.pointer("/state/kind").and_then(|v| v.as_str()) == Some("closed")
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

    /// Whether this device holds a custody (TOTP) lock on `network`'s
    /// governance. Once enrolled, the daemon refuses to *author* a governance
    /// transition (found, admit, promote, evict) without a fresh second-factor
    /// code — so this background found/admit loop, which has no code to give,
    /// must not fire silent `mfa_code: None` proposals that the daemon will only
    /// reject on every startup. Any daemon/parse error reads as *not* enrolled,
    /// so the automatic path keeps working on the common (unlocked) fleet.
    /// `enrolled` is the field [`Request::GovernanceMfaStatus`] returns.
    async fn fleet_mfa_enrolled(self: &Arc<Self>, network: &str) -> bool {
        match self
            .client
            .request(&Request::GovernanceMfaStatus {
                network: network.to_string(),
            })
            .await
        {
            Ok(r) if r.ok => r
                .data
                .and_then(|d| d.get("enrolled").and_then(|v| v.as_bool()))
                .unwrap_or(false),
            _ => false,
        }
    }

    /// This device's **signed** governance role in `network` — `"owner"`,
    /// `"controller"`, or `"member"` — or `None` if it holds none / the state
    /// can't be read. This is the authoritative answer for "what am I on the
    /// fleet": a device the founder *granted* the owner role is an owner here,
    /// even though it isn't the structural key-holder ([`Ownership::is_fleet_owner`]).
    /// Owners are owners; there is no second-class owner. `me` is matched in
    /// bare-pubkey form, as the roles map keys it.
    async fn fleet_signed_role(&self, network: &str) -> Option<String> {
        let me = pubkey_part(&self.local_node_id()?).to_string();
        let data = match self
            .client
            .request(&Request::GovernanceState {
                network: network.to_string(),
            })
            .await
        {
            Ok(r) if r.ok => r.data.unwrap_or(Value::Null),
            _ => return None,
        };
        data.get("state")
            .and_then(|v| v.get("roles"))
            .and_then(|v| v.as_object())
            .and_then(|roles| {
                roles
                    .iter()
                    .find(|(k, _)| pubkey_part(k) == me)
                    .and_then(|(_, v)| v.as_str())
                    .map(str::to_string)
            })
    }

    /// The fleet network's governed topology — the owner-signed,
    /// network-wide shape (daemon ≥ 0.2.36) — as the raw snapshot JSON
    /// (`{"kind":"hubs","hubs":[…],"spoke_redundancy":…}` etc). `None`
    /// when the network isn't governed, the daemon predates governed
    /// topology (no `topology` key in the snapshot), or the state can't
    /// be read — callers treat all three as "no governed shape".
    async fn fleet_governed_topology(&self, network: &str) -> Option<Value> {
        let data = match self
            .client
            .request(&Request::GovernanceState {
                network: network.to_string(),
            })
            .await
        {
            Ok(r) if r.ok => r.data.unwrap_or(Value::Null),
            _ => return None,
        };
        let topo = data.get("state")?.get("topology")?.clone();
        if topo.is_null() {
            None
        } else {
            Some(topo)
        }
    }

    /// True if this device is a **signed owner** of `network` — granted the
    /// owner role in the governance log — regardless of whether it minted the
    /// fleet key. Management authority (evict, admit, promote) keys on this, not
    /// on the structural key-holder check, so a granted owner is a full owner.
    async fn is_fleet_owner_signed(&self, network: &str) -> bool {
        self.fleet_signed_role(network).await.as_deref() == Some("owner")
    }

    /// True if this device holds the **manager** (controller) role in `network`.
    async fn is_fleet_manager(&self, network: &str) -> bool {
        self.fleet_signed_role(network).await.as_deref() == Some("controller")
    }

    /// The device ids (bare pubkey form) that already hold *any* signed role in
    /// `network`'s governance log — owners, controllers, and members alike.
    ///
    /// The fleet-admit path uses this to sign in only members the log doesn't
    /// already carry. Re-granting `member` to a device already in the log is a
    /// redundant transition at best and, for one we'd promoted to
    /// controller/owner, a *demotion* back to member. On any daemon/parse error
    /// this returns the empty set, so the caller falls back to re-asserting the
    /// grant — idempotent and safe, just chattier than necessary.
    async fn signed_role_holders(
        self: &Arc<Self>,
        network: &str,
    ) -> std::collections::HashSet<String> {
        let data = match self
            .client
            .request(&Request::GovernanceState {
                network: network.to_string(),
            })
            .await
        {
            Ok(r) if r.ok => r.data.unwrap_or(Value::Null),
            _ => return std::collections::HashSet::new(),
        };
        data.get("state")
            .and_then(|s| s.get("roles"))
            .and_then(|v| v.as_object())
            .map(|roles| roles.keys().map(|k| pubkey_part(k).to_string()).collect())
            .unwrap_or_default()
    }

    /// The canonical device ids the fleet's signed logs have **removed** —
    /// evicted (or member-tier revoked), the authoritative "no longer in the
    /// fleet" set the daemon projects from the member log. `ensure_fleet_network`
    /// uses it to prune this owner's local claimed-list of a device *another*
    /// owner evicted, whose eviction converged the signed roster but never
    /// reached this device's local list — so the background admit loop stops
    /// resurrecting it. Empty on any daemon/parse error (and against an older
    /// daemon that doesn't report the field), so a transient read failure or a
    /// version skew never prunes a live member — it just falls back to the old
    /// (re-asserting) behaviour.
    async fn signed_evicted(
        self: &Arc<Self>,
        network: &str,
    ) -> std::collections::HashMap<String, u64> {
        let data = match self
            .client
            .request(&Request::GovernanceState {
                network: network.to_string(),
            })
            .await
        {
            Ok(r) if r.ok => r.data.unwrap_or(Value::Null),
            _ => return std::collections::HashMap::new(),
        };
        eviction_stamps(&data)
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

    /// Fold one peer's passive clock-skew sample (from its presence advert's
    /// `sent_at` stamp) into the network verdict, and raise / clear the
    /// out-of-sync warning on the transitions.
    ///
    /// The estimate is the conservative median across peers with a fresh
    /// sample, so one machine with a broken clock reads as *that peer's*
    /// problem (its own node warns, against all of *its* peers) — only when
    /// the majority of the network disagrees with us the same way does this
    /// device conclude its own clock is off. Motivated by real damage: the
    /// fleet's signed member-log converges last-writer-wins on wall-clock
    /// stamps (a skewed clock can strand a device evicted — the "remote
    /// control silently refused" failure), custody TOTP tolerates ±30 s,
    /// and cross-device timestamps stop lining up. Entirely passive — built
    /// from adverts that were flowing anyway, no extra calls to any node.
    fn note_peer_clock(&self, peer: &str, sample_ms: i64) {
        use std::sync::atomic::Ordering;
        const SAMPLE_TTL: std::time::Duration = std::time::Duration::from_secs(15 * 60);
        let (estimate, peers) = {
            let mut map = self.peer_clock_skew.lock();
            map.insert(peer.to_string(), (sample_ms, std::time::Instant::now()));
            map.retain(|_, (_, at)| at.elapsed() < SAMPLE_TTL);
            let samples: Vec<i64> = map.values().map(|(s, _)| *s).collect();
            (conservative_median(&samples), samples.len())
        };
        let Some(skew_ms) = estimate else { return };
        let warned = self.clock_skew_warned.load(Ordering::SeqCst);
        if !warned && skew_ms.abs() >= CLOCK_SKEW_WARN_MS {
            self.clock_skew_warned.store(true, Ordering::SeqCst);
            let secs = skew_ms.abs() as f64 / 1000.0;
            let direction = if skew_ms > 0 { "behind" } else { "ahead of" };
            let message = if peers >= 2 {
                format!(
                    "This device's clock is ~{secs:.0}s {direction} the rest of the network — \
                     fleet roster updates and cross-device timestamps can misbehave. Sync this \
                     machine's clock (NTP)."
                )
            } else {
                format!(
                    "This device's clock and its peer's disagree by ~{secs:.0}s — one of the \
                     two is wrong. Sync both machines' clocks (NTP)."
                )
            };
            tracing::warn!("{message} (skew {skew_ms} ms across {peers} peer(s))");
            self.sink.emit(
                "allmystuff://clock-skew",
                serde_json::json!({
                    "state": "warn",
                    "skew_ms": skew_ms,
                    "peers": peers,
                    "message": message,
                    "source": "presence",
                }),
            );
        } else if warned && skew_ms.abs() <= CLOCK_SKEW_CLEAR_MS {
            self.clock_skew_warned.store(false, Ordering::SeqCst);
            tracing::info!("this device's clock is back in sync with the network");
            self.sink.emit(
                "allmystuff://clock-skew",
                serde_json::json!({
                    "state": "clear",
                    "skew_ms": skew_ms,
                    "peers": peers,
                    "message": "This device's clock is back in sync with the network.",
                    "source": "presence",
                }),
            );
        }
    }

    /// Send this node's presence profile straight to one peer — the
    /// targeted half of `broadcast_presence`, for a peer that just
    /// connected or restarted and so has never heard us.
    async fn send_presence_to(&self, peer: &str) {
        let profile = { self.state.lock().profile.clone() };
        let Some(mut profile) = profile else { return };
        // Same send-time stamp as `broadcast_presence` — a passive
        // clock-skew sample for the receiver.
        profile.sent_at = unix_now_ms();
        let Some(network) = self.network_for_peer(peer) else {
            return;
        };
        // On a CEC room, the profile goes only to peers with a deliberate
        // CEC relationship — the targeted mirror of `broadcast_presence`
        // skipping the rooms entirely. (A session peer normally implies the
        // relationship already; this also covers strangers inherited from a
        // pre-gate session.)
        if crate::cec::is_cec_network(&network) && !self.cec.relationship_with(peer) {
            return;
        }
        // Same per-network claimable scoping as `broadcast_presence`.
        profile.claimable = profile.claimable && self.claimable_advertised_on(&network);
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
                // A deliberate leave: purge the signed governance state + roster
                // so a later rejoin can't reload a stale (forked) genesis.
                .request(&Request::NetworkRemove {
                    network,
                    purge: true,
                })
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

    /// Danger Zone: leave the fleet **and** forget every network on the daemon —
    /// clears this device's fleet membership and purges each mesh's roster +
    /// signed governance state, keeping the device identity. A clean networking
    /// slate for a wedged node. Leaves the fleet first (clears our ownership +
    /// purges the fleet's closed network), then tells the daemon to forget the
    /// rest; the daemon exits so a fresh one reloads clean, and the GUI restarts
    /// the app around it. Best-effort per step — we're resetting regardless.
    pub async fn reset_networking(self: &Arc<Self>) -> Result<(), String> {
        let _ = self.fleet_leave().await;
        if let Err(e) = self.client.request(&Request::ForgetAllNetworks).await {
            // A pre-reset-op daemon can't parse it; the fleet leave above still
            // did the important part. Surface it but don't fail the reset.
            tracing::warn!("reset networking: daemon forget-all errored: {e}");
        }
        Ok(())
    }

    /// Danger Zone: factory reset — wipe this device back to brand-new. Clears
    /// our local ownership record first (so the node can't re-persist
    /// `allmystuff-ownership.json` after the daemon deletes it), then tells the
    /// daemon to wipe its **entire** state directory (`~/.myownmesh`: identity,
    /// config, every network, and our co-located ownership file) and exit. The
    /// GUI restarts the app; a fresh node + daemon come up on empty state with a
    /// new identity. The daemon's response may race its own exit, so a transport
    /// error after the request is treated as "reset underway", not a failure.
    pub async fn factory_reset(self: &Arc<Self>) -> Result<(), String> {
        // Quiesce our ownership writer so it can't rewrite the file the daemon is
        // about to delete. Best-effort — the daemon wipe is the authority, and
        // we're restarting the whole stack regardless.
        let _ = self.ownership.leave_fleet();
        self.emit_owned().await;
        match self.client.request(&Request::FactoryReset).await {
            Ok(_) => Ok(()),
            Err(e) => {
                tracing::warn!(
                    "factory reset: daemon request errored (it is likely already exiting): {e}"
                );
                Ok(())
            }
        }
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
        let network = self
            .ownership
            .fleet_network_id()
            .ok_or("this device isn't in a fleet")?;
        // Authority mirrors the daemon's Evict quorum, keyed on the **signed**
        // role — not the structural key-holder. A signed owner (even one the
        // founder granted, not the key-minter) may evict anyone; a manager may
        // evict managers/members. Gating on the structural `is_fleet_owner`
        // alone made a granted owner a second-class owner that couldn't evict.
        // The daemon is the final arbiter (it rejects an under-powered evict);
        // this local check just avoids a doomed request.
        let structural_owner = self.ownership.is_fleet_owner();
        if !structural_owner
            && !self.is_fleet_owner_signed(&network).await
            && !self.is_fleet_manager(&network).await
        {
            return Err("only a fleet owner or a manager can remove a device".into());
        }
        // Keep the owner's local re-admit list honest so a kicked device isn't
        // re-admitted next `ensure` — a no-op for a manager (empty list). The
        // returned id equals `network`; we keep the one from `fleet_network_id`.
        self.ownership.kick_member(&device)?;
        let target = pubkey_part(&device).to_string();
        // Tell the device directly FIRST, while it's still a live peer on the
        // fleet mesh. The `Evict` below ratifies synchronously on this daemon
        // and drops the peer session, so a `Release` sent afterwards would
        // find no delivery path — the device would be evicted from everyone's
        // roster but never told to reset itself. Order matters for the KVM
        // *unclaim*: a cooperative device must receive this to leave its
        // meshes and return to claim mode. A lost/stolen device simply
        // ignores it; the propagating `Evict` still does its job.
        let _ = self
            .send_control(
                &device,
                &ControlMessage::Ownership(OwnershipControl::Release),
            )
            .await;
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

    /// Front-end command: grant `device` a fleet role. `role` is the UI term
    /// — "manager" (a controller: can admit members) or "owner" (full
    /// authority, co-signs governance). Authoring a role grant is an owner
    /// authority act on the closed network; the daemon enforces the quorum and
    /// rejects the proposal if this device lacks the authority, so we just
    /// float it and surface any refusal. The roster's role projection updates
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
        // Map the UI's "manager" onto MyOwnMesh's "controller".
        let role = match role.as_str() {
            "manager" | "controller" => "controller",
            "owner" => "owner",
            other => return Err(format!("unknown fleet role: {other}")),
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

    /// Front-end command: designate the fleet's infra hubs — the owner-signed,
    /// network-wide shape every member's daemon converges onto (≥ 0.2.36).
    /// A non-empty `hubs` proposes the hub tier (hubs full-mesh each other,
    /// every other member rides `redundancy` of them); an empty list proposes
    /// `full_mesh`, the shape a fleet has before anyone designates hubs. The
    /// daemon enforces owner authority; we float the proposal and surface any
    /// refusal — including the "op unknown" parse error a pre-0.2.36 daemon
    /// gives back, translated into an update hint.
    pub async fn fleet_set_hubs(
        self: &Arc<Self>,
        hubs: Vec<String>,
        redundancy: Option<u32>,
        code: Option<String>,
    ) -> Result<(), String> {
        let network = self
            .ownership
            .fleet_network_id()
            .ok_or("this device isn't in a fleet")?;
        let canon: Vec<String> = hubs
            .iter()
            .map(|h| pubkey_part(h).to_string())
            .filter(|h| !h.is_empty())
            .collect();
        let (topology, hub) = if canon.is_empty() {
            ("full_mesh".to_string(), None)
        } else {
            let spec = match redundancy {
                Some(r) => format!("{}:{r}", canon.join(",")),
                None => canon.join(","),
            };
            ("hubs".to_string(), Some(spec))
        };
        tracing::info!(
            "proposing fleet topology {topology} ({} hubs) on {network}",
            canon.len()
        );
        let resp = self
            .client
            .request(&Request::GovernanceProposeTopology {
                network,
                topology,
                hub,
                mfa_code: code,
            })
            .await;
        match resp {
            Ok(r) if r.ok => {}
            Ok(r) => {
                let msg = r
                    .error
                    .unwrap_or_else(|| "couldn't set the fleet topology".into());
                // A pre-0.2.36 daemon can't parse the op at all — its serde
                // error reads like gibberish in the UI, so translate it.
                if msg.contains("unknown variant") || msg.contains("expected one of") {
                    return Err(
                        "the mesh daemon on this device predates governed topology — \
                         it needs 0.2.36+ (it self-updates shortly after release)"
                            .into(),
                    );
                }
                return Err(msg);
            }
            Err(e) => return Err(e.to_string()),
        }
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
        // A network reset (one disabled, removed, or left — its config_id is
        // gone from the joined set) leaves behind ghosts: peers and the
        // network-derived data we cached for them while it was up. Drop those
        // now so the graph reflects reality. This clears *network* data only —
        // long-lived state (shares, fleet membership + the signed-roster cache,
        // the saved networks, exposed sites) is untouched (see
        // [`Mesh::prune_unjoined_peers`]).
        self.prune_unjoined_peers().await;
        self.subscribe_channels(client_id, &networks).await;
        // The joined set changed — re-learn each connected peer's network from
        // the daemon peer list so a peer reachable only on a newly-arrived or
        // re-enabled mesh (e.g. the fleet network) is addressed there, not the
        // primary fallback.
        self.refresh_peer_networks().await;
        // The joined set just changed (a create / join / import / re-enable, or
        // the fleet network arriving). Reconcile open-mesh policy now so a mesh
        // doesn't wait for the next ownership broadcast to drop its approval
        // gate — in particular a legacy mesh just **re-enabled** from its parked
        // config (which kept `auto_approve: false`) would otherwise reject
        // joiners with no UI to admit them until that later pass.
        self.ensure_open_meshes_auto_approve().await;
        self.advertise_capabilities().await;
        self.broadcast_presence().await;
        self.emit_snapshot();
    }

    /// Clear the ephemeral, network-derived caches for peers no longer
    /// reachable on any joined network — what a network reset (a disabled,
    /// removed, or left network) leaves stale. For each such peer we drop the
    /// live session entry (tearing down any routes to it) and its per-peer
    /// presence caches: the last-seen network, advertised features, and boot
    /// id. A peer still reachable on a network that survived the reset keeps
    /// its caches and re-converges on its next advert; one only on the gone
    /// network is forgotten outright.
    ///
    /// Deliberately scoped to *network* data. Long-lived state survives a
    /// reset untouched: durable shares ([`Mesh::shares`]), fleet membership and
    /// its closed-network signed-roster cache ([`Mesh::ownership`] /
    /// [`Mesh::fleet_authorized`]), the saved network configs, and the exposed
    /// sites set are all per-device or per-person, not per-network, so a
    /// network coming and going never drops them.
    async fn prune_unjoined_peers(self: &Arc<Self>) {
        let (effects, dropped) = {
            let mut st = self.state.lock();
            let joined: std::collections::HashSet<String> = st.networks.iter().cloned().collect();
            // Peers whose last-seen network is gone from the joined set.
            let stale: std::collections::HashSet<String> = st
                .peer_networks
                .iter()
                .filter(|(_, net)| !joined.contains(net.as_str()))
                .map(|(peer, _)| peer.clone())
                .collect();
            if stale.is_empty() {
                return;
            }
            for peer in &stale {
                st.peer_networks.remove(peer);
                st.peer_features.remove(peer);
                st.peer_boots.remove(peer);
            }
            // Drop the same peers (matched by canonical pubkey) from the live
            // session, tearing down any routes to them.
            let mut effects = Vec::new();
            let mut dropped = 0usize;
            if let Some(session) = st.session.as_mut() {
                let gone: Vec<NodeId> = session
                    .peers()
                    .filter(|p| stale.contains(pubkey_part(p.node.as_str())))
                    .map(|p| p.node.clone())
                    .collect();
                for id in gone {
                    effects.extend(session.drop_peer(&id));
                    dropped += 1;
                }
            }
            (effects, dropped)
        };
        if dropped > 0 {
            tracing::info!("network reset: cleared {dropped} stale peer(s) from a removed network");
            // We just threw away everything we knew about those peers (their
            // profile, features, network, boot id). As far as their state goes
            // we're now a fresh incarnation, so refresh our boot id: the *next*
            // presence advert carries a new one, which is what makes a peer
            // that never reset — same boot id on file, still holding us as a
            // `known` peer — actually re-send its state instead of treating our
            // advert as old news. This is the fix for "refresh on one side
            // breaks the connection until *both* sides refresh": without it the
            // resetting side discarded its caches but the other side never
            // re-fed them.
            self.boot_id.store(fresh_boot_id(), Ordering::Relaxed);
        }
        // Boxed to break the async-fn cycle: `process_effects` can route back
        // through ownership/`sync_networks`, and without indirection the
        // `sync_networks` → `prune_unjoined_peers` → `process_effects` chain
        // would be an infinitely-sized future.
        Box::pin(self.process_effects(effects)).await;
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
            // CEC Support rides the same engine on its own channels; subscribing
            // everywhere is harmless (they're empty on non-CEC meshes) and means
            // a CEC Silent mesh is live for connect-requests the moment it's
            // joined.
            allmystuff_cec_protocol::CHANNEL_CONTROL,
            allmystuff_cec_protocol::CHANNEL_PRESENCE,
        ];
        for network in networks {
            for channel in channels {
                // A failed subscribe used to be discarded (`let _ =`) —
                // and one transient refusal meant presence/control/media on
                // that network were dead for the whole session, silently:
                // peers never appeared, offers to us were dropped
                // daemon-side, nothing logged. Retry a couple of times with
                // a beat between, and if it still fails, say exactly which
                // network+channel is dark — a half-subscribed mesh must be
                // diagnosable from the log.
                let mut ok = false;
                for attempt in 0..3u32 {
                    if attempt > 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(
                            500 * u64::from(attempt),
                        ))
                        .await;
                    }
                    match self
                        .client
                        .request(&Request::ChannelSubscribe {
                            client_id,
                            network: network.clone(),
                            channel: channel.to_string(),
                        })
                        .await
                    {
                        Ok(resp) if resp.ok => {
                            ok = true;
                            break;
                        }
                        Ok(resp) => {
                            tracing::warn!(
                                network = %network,
                                channel = %channel,
                                "channel subscribe refused: {}",
                                resp.error.as_deref().unwrap_or("(no error)")
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                network = %network,
                                channel = %channel,
                                "channel subscribe failed: {e}"
                            );
                        }
                    }
                }
                if !ok {
                    tracing::error!(
                        network = %network,
                        channel = %channel,
                        "channel is DARK for this session — peers on this mesh won't see us \
                         on it (presence/control/media affected); a daemon-link reconnect \
                         will retry the full bring-up"
                    );
                }
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
                    // simultaneous streams to one peer can ride separate lanes,
                    // and whether it speaks the binary media pipes (a capability
                    // flag, since the feature predates a release and the version
                    // pin can't gate it). Both come off the same Status.
                    if let Some(d) = self
                        .client
                        .request(&Request::Status)
                        .await
                        .ok()
                        .and_then(|r| r.data)
                    {
                        if let Some(n) = d.get("media_lanes").and_then(|v| v.as_u64()) {
                            self.daemon_lanes
                                .store(n.clamp(1, 255) as u8, Ordering::SeqCst);
                        }
                        let pipes = d
                            .get("media_pipes")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        self.daemon_media_pipes.store(pipes, Ordering::SeqCst);
                        if !pipes {
                            tracing::info!(
                                "daemon has no binary media pipes — H.264/Opus ride the base64 video_send/audio_send path (rebuild myownmesh from this branch to enable the binary pipes)"
                            );
                        }
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
        // Compare endpoints to ourselves *canonically* — the route's ids carry
        // the UI's display suffix while `me` is the bare node id. Without this
        // a loopback (e.g. a local terminal) matches neither the loopback arm
        // nor the host/viewer arms, and nothing starts. The bare ids only feed
        // `== me` checks, log labels, and the peer arg to the capture starts
        // (which the routing layer canonicalises again), so normalising them
        // here is safe.
        let me = pubkey_part(&me).to_string();
        let from_node = pubkey_part(&node_of(route.from.as_str())).to_string();
        let to_node = pubkey_part(&node_of(route.to.as_str())).to_string();

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
                                // Decoder drop only — NOT release_audio_lanes:
                                // the route is still live (it keeps its rank in
                                // the peer's Opus order), so the positional
                                // close there would hit the top-ranked
                                // neighbour's lane, not this route's.
                                self.audio_decoders.lock().remove(&route.id);
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
                    // Tell the viewer the pinned lane (best-effort, off the
                    // sync start path; the pin is already assigned above).
                    let (mesh, rid, peer) = (self.clone(), route.id.clone(), to_node.clone());
                    crate::spawn(async move { mesh.announce_video_lane(&rid, &peer).await });
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
                    let (mesh, rid, peer) = (self.clone(), route.id.clone(), to_node.clone());
                    crate::spawn(async move { mesh.announce_video_lane(&rid, &peer).await });
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
            MediaKind::Storage if is_mapped_drive_route(route) => {
                if from_node == me && to_node != me {
                    match self
                        .files
                        .mapped_root(&route.id)
                        .or_else(|| mapped_drive_root(route, &me))
                    {
                        Some(root) => {
                            self.files.map_root(&route.id, root.clone());
                            tracing::info!(
                                "route {} active — mapping {} to {}",
                                route.id,
                                root.display(),
                                short_id(&to_node)
                            );
                        }
                        None => {
                            tracing::warn!(
                                "route {} refused — source is not a mounted local volume",
                                route.id
                            );
                            let mesh = self.clone();
                            let rid = route.id.clone();
                            crate::spawn(async move {
                                let _ = mesh.disconnect(rid).await;
                            });
                        }
                    }
                } else if to_node == me && from_node != me {
                    self.files.ensure_queue(&route.id);
                    let drive = self
                        .state
                        .lock()
                        .session
                        .as_ref()
                        .and_then(|session| session.route(&route.id))
                        .and_then(|live| live.drive.clone())
                        .unwrap_or(DriveRouteOffer {
                            label: "Remote drive".into(),
                            root: None,
                            mount: String::new(),
                            request: None,
                        });
                    let mesh = self.clone();
                    let mounts = self.drive_mounts.clone();
                    let route_id = route.id.clone();
                    crate::spawn(async move {
                        match mounts
                            .mount(mesh.clone(), route_id.clone(), drive.label, drive.mount)
                            .await
                        {
                            Ok(info) => {
                                if let Some(intent) =
                                    mesh.drive_reconnects.lock().get_mut(&route_id)
                                {
                                    // Empty means "pick one" only on the first
                                    // mount. A reconnect must reclaim the same
                                    // letter/mount point the user already sees.
                                    intent.mount = info.mount.clone();
                                }
                                mesh.persist_drive_reconnects();
                                tracing::info!(
                                    "route {} active — native drive {} mounted from {}",
                                    route_id,
                                    info.mount,
                                    short_id(&from_node)
                                );
                                mesh.sink.emit(
                                    "allmystuff://drive-mount",
                                    json!({
                                        "route": route_id,
                                        "from": from_node,
                                        "mount": info.mount,
                                        "label": info.label,
                                        "error": null,
                                    }),
                                );
                            }
                            Err(error) => {
                                tracing::warn!(
                                    "route {} native drive mount failed: {error}",
                                    route_id
                                );
                                mesh.sink.emit(
                                    "allmystuff://drive-mount",
                                    json!({
                                        "route": route_id,
                                        "from": from_node,
                                        "error": error,
                                    }),
                                );
                                let _ = mesh.disconnect(route_id).await;
                            }
                        }
                    });
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

    /// The ids of the **active** codec media routes between us and `peer` in
    /// one direction, sorted — the shared, signalling-free basis for lane
    /// assignment: both ends compute the identical list from their own copy of
    /// the session, so a route lands on the same lane on both. `codec` is
    /// "h264" (video) or "opus" (audio); `outbound` = we are the source.
    ///
    /// Only **active** routes count. A route still negotiating (Offered /
    /// Incoming) or already torn down must not occupy a lane slot: it carries
    /// no media, yet — being in `routes()` — it would shift every later
    /// route's index and so its lane, decoding a live stream's frames into
    /// the wrong window for as long as the transient lasts. Restricting the
    /// basis to active routes keeps the two ends agreeing on a stable lane for
    /// the whole life of each stream (both ends process Active/Teardown), so
    /// an unrelated route coming or going no longer reshuffles a live one.
    /// The capability list this node advertises. On a `host` build it is the
    /// bridge's list verbatim. A capture-less build (iOS) strips the sources
    /// it cannot serve — the synthetic screen and any camera — so peers are
    /// never invited to open a stream the stub planes would refuse. Sinks
    /// (video-view, audio out) and the mic (real under `audio-io`) stay.
    fn advertised_capabilities(
        inv: &allmystuff_inventory::Inventory,
        node: &allmystuff_graph::NodeId,
    ) -> Vec<allmystuff_graph::Capability> {
        #[allow(unused_mut)]
        let mut caps =
            allmystuff_bridge::capabilities_with_screens(inv, node, &crate::video::extra_screens());
        #[cfg(not(feature = "host"))]
        caps.retain(|c| c.origin != "screen" && c.origin != "camera");
        caps
    }

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
            .active_routes()
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

    /// Pin (or look up) the RTP video track lane an outbound H.264 route to
    /// `peer` streams on — the **lowest free** lane in the peer's pool among
    /// that peer's already-pinned routes, held for the route's lifetime.
    /// `None` when the pool is exhausted or the daemon has no video lane (the
    /// route then rides MJPEG). Called once when the stream's transport is
    /// chosen; thereafter [`Self::video_lane`] just reads the pin.
    ///
    /// Pinning is what makes the lane stable: a second feed opening (or a
    /// third tearing down) no longer renumbers a live feed's lane, so the
    /// viewer — told the binding over [`RouteControl::VideoLane`] — never
    /// briefly maps one monitor's frames onto another's window.
    fn assign_video_lane(&self, peer: &str, route_id: &str) -> Option<u8> {
        let cap = self.effective_video_lanes(peer);
        if cap == 0 {
            return None;
        }
        let peer_canon = pubkey_part(peer);
        // The whole get/compute/insert runs under the pin lock — two screens
        // activating at once can never both pick "lane 0" (the lock serialises
        // us; the second sees the first's pin). Sampling the live session for
        // the taken lanes instead raced: it was read before the lock, so a
        // sibling route not yet visible there left its lane looking free, and
        // both screens collapsed onto one track.
        let mut pins = self.video_lane_pins.lock();
        let lane = free_lane_for_peer(&pins, peer_canon, route_id, cap)?;
        pins.insert(route_id.to_string(), lane);
        Some(lane)
    }

    /// The video track lane an outbound H.264 route to `peer` is streaming on:
    /// the lane [`Self::assign_video_lane`] pinned at stream start. `None` once
    /// the route has torn down (its pin freed) — the forwarder then drops the
    /// frame rather than guessing a lane. `outbound` is kept for symmetry with
    /// the audio twin; the receive side resolves lanes via
    /// [`Self::video_route_for_lane`], never here.
    fn video_lane(&self, route_id: &str, peer: &str, outbound: bool) -> Option<u8> {
        if outbound {
            return self.video_lane_pins.lock().get(route_id).copied();
        }
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

    /// Record the lane→route binding a streamer announced
    /// ([`RouteControl::VideoLane`]) so inbound H.264 on that lane routes to
    /// the right console window regardless of the local route order.
    fn record_video_lane(&self, peer: &str, route_id: &str, lane: u8) {
        let canon = pubkey_part(peer).to_string();
        let mut binds = self.video_lane_binds.lock();
        let per_peer = binds.entry(canon).or_default();
        // A lane is reused only after its previous route tore down (which
        // clears its binding), so overwriting here just records the current
        // owner; drop any other lane that stale-pointed at this same route.
        per_peer.retain(|l, r| *l == lane || r != route_id);
        per_peer.insert(lane, route_id.to_string());
    }

    /// The route whose inbound video samples arrive on `lane` from `peer`.
    ///
    /// Once a peer has announced *any* lane binding ([`Self::record_video_lane`])
    /// the announced map is **authoritative**: this lane is whatever it bound,
    /// or — if it hasn't bound this lane yet — `None`. We deliberately do NOT
    /// fall back to a positional guess there: the streamer pins lanes
    /// non-positionally (lowest-free), so guessing by sorted position would put
    /// one monitor's frames in another monitor's window (and `None` simply
    /// leaves that window holding its last frame until the real binding lands).
    ///
    /// Only a peer that has announced *nothing* (an older build that doesn't
    /// pin/announce, or the brief moment before its first announce) uses the
    /// positional sort — exactly the pre-binding behaviour.
    fn video_route_for_lane(&self, peer: &str, lane: u8) -> Option<String> {
        let canon = pubkey_part(peer);
        {
            let binds = self.video_lane_binds.lock();
            if let Some(per_peer) = binds.get(canon) {
                return per_peer.get(&lane).cloned();
            }
        }
        // No binding announced yet (a fresh peer, or every lane freed when the
        // last route to it tore down). Positional over the peer's active h264
        // routes — the pre-binding behaviour.
        if let Some(r) = self
            .sorted_media_routes(peer, false, "h264")
            .into_iter()
            .nth(lane as usize)
        {
            return Some(r);
        }
        // Re-open fallback. On a re-open the SAME route id comes back and the
        // sender re-establishes its RTP track — so samples land on the lane
        // again — BEFORE the daemon's session re-tags that route's codec as
        // h264. The positional filter above keys on that tag, so it misses the
        // re-opened route and the frames are dropped into the void ("connecting
        // forever" on the second open). The console IS watching the route, so
        // map by position over the video routes we actually watch from this
        // peer — knowledge that doesn't depend on the re-tag timing. Position
        // keeps multi-monitor correct, and an authoritative binding (above)
        // still wins the instant the streamer's VideoLane announce lands.
        let mut watched = self.watched_video_routes_from(canon);
        watched.sort_unstable();
        watched.into_iter().nth(lane as usize)
    }

    /// Route ids of the inbound video routes this viewer currently watches whose
    /// streamer is `canon` (bare pubkey) — the re-open fallback for
    /// [`Self::video_route_for_lane`]. Cheap: the watcher map holds one entry
    /// per open console stream.
    fn watched_video_routes_from(&self, canon: &str) -> Vec<String> {
        self.video_watchers
            .lock()
            .keys()
            .filter(|rid| {
                rid.strip_prefix("route:")
                    .and_then(|s| s.split_once('→'))
                    .map(|(from, _)| pubkey_part(&node_of(from)) == canon)
                    .unwrap_or(false)
            })
            .cloned()
            .collect()
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
    /// Bounded wait for the daemon's video bring-up before a one-shot
    /// transport decision. Dials are fast now (the area dial has no
    /// discovery pause), and racing the VideoSubscribe probe stripped
    /// h264 and pinned capable pairs on MJPEG for the whole session. A
    /// daemon that truly predates the track lane never flips the flag —
    /// the timeout falls through to the honest MJPEG pick.
    async fn await_video_bringup(&self) {
        const DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);
        let by = std::time::Instant::now() + DEADLINE;
        while !self.daemon_video.load(Ordering::SeqCst) && std::time::Instant::now() < by {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

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
        // Pin a track lane for this route now (lowest free in the peer's
        // pool). A pin is what lets us tell the viewer a stable binding; no
        // pin (pool exhausted / no daemon lane) means MJPEG, exactly as v1.
        if accepts_h264 && self.assign_video_lane(to_node, &route.id).is_some() {
            VideoMode::H264
        } else {
            VideoMode::Mjpeg
        }
    }

    /// Tell the viewer which video track lane this route streams on, so it
    /// demuxes inbound H.264 by the announced binding instead of a positional
    /// guess. No-op for an MJPEG route (no pinned lane). Best-effort: a viewer
    /// that never hears it (older build, a dropped message) falls back to the
    /// positional lane, exactly as before.
    async fn announce_video_lane(&self, route_id: &str, peer: &str) {
        let Some(lane) = self.video_lane(route_id, peer, true) else {
            return;
        };
        if let Err(e) = self
            .send_control(
                peer,
                &ControlMessage::Route(RouteControl::VideoLane {
                    route_id: route_id.to_string(),
                    lane,
                }),
            )
            .await
        {
            tracing::debug!("announcing video lane for {route_id} failed: {e}");
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
        let generation = self.begin_video_generation(&route_id);
        let recovery = Arc::new(VideoRecovery::new(&route_id));
        // The LAN gate: the automatic fps/bitrate dials open up only on a
        // link the daemon has classified host↔host. Unknown (ICE not yet
        // introspected) starts conservative; the nudge below upgrades the
        // live stream as soon as the class lands.
        let link = {
            let st = self.state.lock();
            st.peer_links
                .get(pubkey_part(to_node))
                .copied()
                .unwrap_or_default()
        };
        if link == crate::video::LinkClass::Unknown {
            // The class usually lands within a couple of seconds of ICE
            // settling — poll the daemon shortly after the stream starts so
            // a LAN viewer isn't stuck on the conservative dials until the
            // next natural refresh (peer approval / snapshot).
            let mesh = Arc::downgrade(self);
            crate::spawn(async move {
                for delay_ms in [2_000u64, 6_000] {
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                    let Some(mesh) = mesh.upgrade() else { return };
                    mesh.refresh_peer_networks().await;
                }
            });
        }
        // Queue admission is not delivery. Recovery begins on either a full
        // producer queue or a downstream lane/write failure, holds dependent
        // deltas at both sides of the queue, and ends only after the current
        // epoch's IDR reaches the existing media pipe successfully.
        let recovery_mesh = Arc::downgrade(self);
        let capture_recovery = recovery;
        self.video.start_capture(
            route.id.clone(),
            mode,
            source,
            crate::video::Tune {
                link,
                ..Default::default()
            },
            move |packet| {
                let key = match &packet {
                    VideoPacket::H264 { key, .. } => Some(*key),
                    VideoPacket::Jpeg(_) => None,
                };
                if capture_recovery.suppresses(key) {
                    if let Some(mesh) = recovery_mesh.upgrade() {
                        capture_recovery.note_suppressed(&mesh);
                    }
                    return false;
                }
                let epoch = capture_recovery.epoch();
                let failure = match tx.try_send((
                    peer.clone(),
                    route_id.clone(),
                    generation,
                    packet,
                    epoch,
                    capture_recovery.clone(),
                )) {
                    Ok(()) => None,
                    Err(mpsc::error::TrySendError::Full(_)) => Some("shared queue full"),
                    Err(mpsc::error::TrySendError::Closed(_)) => Some("shared queue closed"),
                };
                if let Some(reason) = failure {
                    if key.is_some() {
                        if let Some(mesh) = recovery_mesh.upgrade() {
                            capture_recovery.note_drop(&mesh, key, reason);
                        }
                    }
                    false
                } else {
                    true
                }
            },
            move |state, detail| {
                // Capture-state transitions travel to the viewer in-band
                // (`vstat`), so its console can explain a black stage
                // instead of just showing one.
                let Some(mesh) = status_mesh.upgrade() else {
                    return;
                };
                let route = status_route.clone();
                if !mesh.video_generation_is_current(&route, generation) {
                    tracing::debug!(
                        "discarding stale video status for {route} generation {generation}"
                    );
                    return;
                }
                let frame = VideoStatusFrame::new(route.clone(), state, detail);
                let peer = status_peer.clone();
                crate::spawn(async move {
                    if !mesh.video_generation_is_current(&route, generation) {
                        return;
                    }
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
        if !self.sender_may_drive(&peer, DrivePlane::Terminal) {
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
        if !self.sender_may_drive(&peer, DrivePlane::Terminal) {
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
                route_sources_on(&r.route, &me),
                route_sinks_on(&r.route, &me),
            )
        };
        if hosts_here {
            if !self.sender_may_drive(from, DrivePlane::Terminal) {
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
            // Endpoint self-checks compare *canonically*: the UI builds the
            // route's host endpoint from the suffixed display id while `me` is
            // the bare node id, so a raw `==` misses a genuine self-route (see
            // `same_node`). This machine must be the route's viewer…
            if !(r.is_active()
                && is_terminal_route(&r.route)
                && same_node(&node_of(r.route.to.as_str()), &me))
            {
                return Err("route isn't an active terminal session here".into());
            }
            // …and a terminal whose *source* is this machine too has no peer to
            // frame to: the shell is hosted right here, so input/resize go
            // straight to the local PTY rather than out over the mesh. The raw
            // `==` this replaces left a loopback ConPTY blank on Windows — the
            // viewer's cursor-position reply (CSI 6 n) was framed to a
            // non-existent peer, and ConPTY withholds all output until that
            // reply lands.
            let loopback = same_node(&node_of(r.route.from.as_str()), &me);
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
        if !self.sender_may_drive(from, DrivePlane::Terminal) {
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
        let (hosts_here, views_here, shared, mapped) = {
            let st = self.state.lock();
            let Some(r) = st.session.as_ref().and_then(|s| s.route(&frame.route)) else {
                return;
            };
            let shared = is_shared_route(&r.route);
            let mapped = is_mapped_drive_route(&r.route);
            if !(r.is_active()
                && (is_files_route(&r.route) || shared || mapped)
                && pubkey_part(r.peer.as_str()) == pubkey_part(from))
            {
                tracing::debug!(
                    "file frame for {} refused (route not live here)",
                    frame.route
                );
                return;
            }
            (
                route_sources_on(&r.route, &me),
                route_sinks_on(&r.route, &me),
                shared,
                mapped,
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
            // A Storage route is itself the local user's explicit, scoped
            // offer of one volume. Ordinary `:files` remains the privileged
            // whole-machine browser and re-clears owner/fleet/share/CEC.
            if !mapped && !self.sender_may_drive(from, DrivePlane::Files) {
                tracing::warn!("dropped file request from {from}: not an authorized controller");
                return;
            }
            let root = mapped
                .then(|| self.files.mapped_root(&frame.route))
                .flatten();
            if mapped && root.is_none() {
                tracing::warn!("dropped mapped-drive request from {from}: route has no local root");
                return;
            }
            match &frame.event {
                // Upload pieces are applied inline: pieces of one upload
                // must land in arrival order (the viewer sends them
                // sequentially), and a piece is one small append.
                FileEvent::Write { .. } => {
                    if let Some(reply) =
                        crate::files::write_piece_in_root(&frame.event, root.as_deref())
                    {
                        self.send_file_event(frame.route.clone(), from.to_string(), reply);
                    }
                }
                FileEvent::WriteRange { .. } => {
                    if let Some(reply) =
                        crate::files::write_range_in_root(&frame.event, root.as_deref())
                    {
                        self.send_file_event(frame.route.clone(), from.to_string(), reply);
                    }
                }
                FileEvent::Volumes { req } if mapped => {
                    self.send_file_event(
                        frame.route.clone(),
                        from.to_string(),
                        FileEvent::Err {
                            req: *req,
                            reason: "volume inventory is unavailable on a scoped drive route"
                                .into(),
                        },
                    );
                }
                FileEvent::Volumes { .. }
                | FileEvent::List { .. }
                | FileEvent::Read { .. }
                | FileEvent::Stat { .. }
                | FileEvent::ReadRange { .. }
                | FileEvent::Mkdir { .. }
                | FileEvent::Rename { .. }
                | FileEvent::Delete { .. } => {
                    self.start_files_request_in_root(&frame.route, from, frame.event, root);
                }
                // Response kinds (and `Fetch`, which only a `:shared` route
                // serves) landing on the files host are a confused peer.
                _ => {}
            }
        } else if views_here {
            if self.files.deliver_rpc(&frame.route, &frame.event) {
                return;
            }
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
        self.start_files_request_in_root(route_id, peer, event, None);
    }

    fn start_files_request_in_root(
        self: &Arc<Self>,
        route_id: &str,
        peer: &str,
        event: FileEvent,
        root: Option<std::path::PathBuf>,
    ) {
        let mut rx = self.files.handle_in_root(route_id, event, root);
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
        let want_volumes = matches!(event, FileEvent::Volumes { .. });
        if !is_viewer_file_request(&event) {
            return Err("responses come from the host, not the viewer".into());
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
            } else if want_volumes {
                // Volume inventory is machine-wide metadata. A mapped-drive
                // route is deliberately scoped to one offered root.
                is_files_route(&r.route)
            } else {
                is_files_route(&r.route) || is_mapped_drive_route(&r.route)
            };
            if !(r.is_active() && kind_ok && route_sinks_on(&r.route, &me)) {
                return Err("route isn't an active files session here".into());
            }
            r.peer.to_string()
        };
        let seq = self.file_seq.fetch_add(1, Ordering::Relaxed);
        let frame = FileFrame::new(&route_id, seq, event);
        let payload = serde_json::to_value(&frame).map_err(|e| e.to_string())?;
        self.send_media_value(&peer, payload).await
    }

    /// Issue one files-plane request for a native filesystem adapter and
    /// collect its complete reply (one event for metadata/mutations, a
    /// sequence for ranged reads).
    pub(crate) async fn drive_file_request(
        self: &Arc<Self>,
        route_id: &str,
        event: FileEvent,
    ) -> Result<Vec<FileEvent>, String> {
        self.drive_file_request_timeout(route_id, event, Duration::from_secs(30))
            .await
    }

    pub(crate) async fn drive_file_request_timeout(
        self: &Arc<Self>,
        route_id: &str,
        event: FileEvent,
        timeout: Duration,
    ) -> Result<Vec<FileEvent>, String> {
        let req = event.req();
        let mut replies = self.files.begin_rpc(route_id, req);
        let result = tokio::time::timeout(timeout, async {
            self.file_send(route_id.to_string(), event).await?;
            let mut events = Vec::new();
            while let Some(event) = replies.recv().await {
                let terminal = matches!(
                    event,
                    FileEvent::Entries { .. }
                        | FileEvent::Metadata { .. }
                        | FileEvent::Ok { .. }
                        | FileEvent::Err { .. }
                        | FileEvent::Chunk { eof: true, .. }
                );
                events.push(event);
                if terminal {
                    break;
                }
            }
            Ok::<_, String>(events)
        })
        .await;
        self.files.cancel_rpc(route_id, req);
        match result {
            Ok(Ok(events)) if !events.is_empty() => Ok(events),
            Ok(Ok(_)) => Err("mapped drive disconnected before the request completed".into()),
            Ok(Err(error)) => Err(error),
            Err(_) => Err("mapped drive request timed out".into()),
        }
    }

    pub(crate) fn next_file_request_id(&self) -> u64 {
        self.file_seq.fetch_add(1, Ordering::Relaxed)
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
        self.sites
            .list_mappings()
            .into_iter()
            .map(|(_, node, host_port, local_port)| (node, host_port, local_port))
            .collect()
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
                if !self.sender_may_drive(from, DrivePlane::Sites) {
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
        // Already mapped? Hand back the existing local port only while its
        // accept task is alive. A route offer that expires/rejects causes that
        // task to exit and drop the listener; retaining its mapping produced a
        // permanent zombie where every later click reopened dead localhost.
        if let Some((route_id, _, _, local_port)) = self
            .sites
            .list_mappings()
            .into_iter()
            .find(|(_, n, hp, _)| pubkey_part(n) == pubkey_part(&node) && *hp == port)
        {
            let accept_finished = self.sites.mapping_task_finished(&route_id).unwrap_or(true);
            if !accept_finished {
                return Ok(local_port);
            }
            tracing::warn!(
                "site mapping {}:{} on :{} had no listener — rebuilding",
                short_id(&node),
                port,
                local_port
            );
            self.sites.stop_route(&route_id);
            {
                let mut st = self.state.lock();
                if let Some(s) = st.session.as_mut() {
                    let _ = s.teardown(&route_id);
                }
            }
            let listener = self.bind_exact_local_port(local_port).await?;
            self.establish_site_route(node, port, listener, local_port)
                .await?;
            return Ok(local_port);
        }
        // Bind a local listener, preferring the same port number, then a free
        // one — the OS is the final arbiter, so retry on a lost race.
        let (listener, local_port) = self.bind_site_listener(port).await?;
        self.establish_site_route(node, port, listener, local_port)
            .await?;
        Ok(local_port)
    }

    /// Offer a site route for `node`:`host_port` over an already-bound local
    /// `listener` (on `local_port`), start its accept loop, and record the
    /// mapping. The route is minted the same way every time — generic media,
    /// source `<host>:site`, a per-mapping viewer sink — so both a fresh
    /// [`Self::site_map`] and a post-reconnect [`Self::remap_site_route`] speak
    /// the identical contract. Returns the minted route id.
    async fn establish_site_route(
        self: &Arc<Self>,
        node: String,
        host_port: u16,
        listener: tokio::net::TcpListener,
        local_port: u16,
    ) -> Result<String, String> {
        let me = self.local_node_id().ok_or("mesh not ready")?;
        // Mint the route: generic media, source `<host>:site`, sink a
        // per-mapping viewer endpoint (never a catalog capability — shape is
        // the contract, like terminal/files).
        let seq = self.site_seq.fetch_add(1, Ordering::Relaxed);
        let from = format!("{node}:site");
        let to = format!("{me}:site-view:{}-{seq}", host_port);
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
        let accept = self.spawn_site_accept(route_id.clone(), node.clone(), host_port, listener);
        self.sites.add_mapping(
            route_id.clone(),
            ClientMapping::new(node, host_port, local_port, accept),
        );
        Ok(route_id)
    }

    /// Auto-re-map a site whose host just rejected its route — a KVM reconnect
    /// or network change tore the old route down and the host NACKed a stray
    /// frame. Re-offers a fresh route onto the *same* local port so an open
    /// `localhost:<port>` keeps working with no manual unmap/remap. Bounded
    /// retries with a growing backoff: enough to ride out a reconnect, few
    /// enough to give up (rather than loop) if we've genuinely lost access and
    /// the host keeps refusing.
    async fn remap_site_route(self: &Arc<Self>, node: String, host_port: u16, local_port: u16) {
        let key = format!("{}:{}", pubkey_part(&node), host_port);
        if !self.site_remap_inflight.lock().insert(key.clone()) {
            return; // already healing this mapping
        }
        for attempt in 0..SITE_REMAP_ATTEMPTS {
            tokio::time::sleep(SITE_REMAP_BACKOFF.saturating_mul(attempt + 1)).await;
            // A manual remap (or a prior attempt) already restored it.
            if self.sites.route_for(&node, host_port).is_some() {
                break;
            }
            let listener = match self.bind_exact_local_port(local_port).await {
                Ok(l) => l,
                Err(e) => {
                    tracing::debug!("site re-map bind :{local_port} failed: {e}");
                    continue;
                }
            };
            match self
                .establish_site_route(node.clone(), host_port, listener, local_port)
                .await
            {
                Ok(route_id) => {
                    if self.await_route_active(&route_id).await {
                        tracing::info!(
                            "site {}:{} re-mapped on :{} after reconnect",
                            short_id(&node),
                            host_port,
                            local_port
                        );
                        break;
                    }
                    // Host didn't accept in time — clear this attempt fully so
                    // the next one re-binds cleanly, then retry.
                    self.sites.stop_route(&route_id);
                    let mut st = self.state.lock();
                    if let Some(s) = st.session.as_mut() {
                        let _ = s.teardown(&route_id);
                    }
                }
                Err(e) => tracing::debug!("site re-map offer failed: {e}"),
            }
        }
        self.site_remap_inflight.lock().remove(&key);
    }

    /// Bind a loopback listener on *exactly* `port`, retrying briefly — a
    /// just-aborted accept loop may not have released the port yet. The re-map
    /// path needs the identical local port an open tab is already on, so unlike
    /// [`Self::bind_site_listener`] it never falls back to another number.
    async fn bind_exact_local_port(&self, port: u16) -> Result<tokio::net::TcpListener, String> {
        use std::net::{Ipv4Addr, SocketAddr};
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let mut last = String::new();
        for _ in 0..20 {
            match tokio::net::TcpListener::bind(addr).await {
                Ok(l) => return Ok(l),
                Err(e) => {
                    last = e.to_string();
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }
        Err(format!("local port :{port} still busy after 2s: {last}"))
    }

    /// NACK a site frame that arrived on a route we don't hold live — the
    /// symmetric twin of the KVM bridge's `nackDeadRoute`: tell the sender its
    /// route is gone so it re-offers (which [`Self::remap_site_route`] does),
    /// instead of tunnelling into the void. Rate-limited per route so a client
    /// draining a full pipe onto a dead route produces one Reject, not a flood.
    fn nack_dead_site_route(self: &Arc<Self>, from: &str, route: &str) {
        {
            let now = std::time::Instant::now();
            let mut at = self.site_nack_at.lock();
            if let Some(t) = at.get(route) {
                if now.duration_since(*t) < SITE_NACK_COOLDOWN {
                    return;
                }
            }
            // Bound the map across many short-lived route ids.
            if at.len() > 128 {
                at.retain(|_, t| now.duration_since(*t) < SITE_NACK_COOLDOWN * 4);
            }
            at.insert(route.to_string(), now);
        }
        let mesh = self.clone();
        let (from, route) = (from.to_string(), route.to_string());
        crate::spawn(async move {
            let _ = mesh
                .send_control(
                    &from,
                    &ControlMessage::Route(RouteControl::Reject {
                        route_id: route,
                        reason: "route not live on this device — re-offer to reconnect".into(),
                    }),
                )
                .await;
        });
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
                // Do not leave a zombie mapping whose listener disappears with
                // this task. Clear the failed route from a separate task (it
                // owns this task's JoinHandle) and run the existing bounded
                // same-port recovery loop. If this was already one of that
                // loop's attempts, its in-flight guard makes the nested heal a
                // no-op and the parent attempt continues normally.
                if let Some((node, failed_port, local_port)) = mesh.sites.mapping_details(&route_id)
                {
                    let healer = mesh.clone();
                    let failed_route = route_id.clone();
                    crate::spawn(async move {
                        healer.sites.stop_route(&failed_route);
                        {
                            let mut st = healer.state.lock();
                            if let Some(s) = st.session.as_mut() {
                                let _ = s.teardown(&failed_route);
                            }
                        }
                        healer.remap_site_route(node, failed_port, local_port).await;
                    });
                }
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
        let placement = {
            let st = self.state.lock();
            match st.session.as_ref().and_then(|s| s.route(&frame.route)) {
                Some(r)
                    if r.is_active()
                        && is_site_route(&r.route)
                        && pubkey_part(r.peer.as_str()) == pubkey_part(from) =>
                {
                    Some((
                        route_sources_on(&r.route, &me),
                        route_sinks_on(&r.route, &me),
                    ))
                }
                _ => None,
            }
        };
        let Some((hosts_here, views_here)) = placement else {
            tracing::debug!(
                "site frame for {} refused (route not live here)",
                frame.route
            );
            // Tell the sender its route is gone so it re-offers, instead of
            // tunnelling into the void — the symmetric twin of the KVM bridge's
            // nackDeadRoute. Rate-limited per route; sent off the state lock.
            self.nack_dead_site_route(from, &frame.route);
            return;
        };

        if hosts_here {
            // The proxy *into* this machine — as privileged as the terminal,
            // so the same owner/fleet gate, re-cleared per frame.
            if !self.sender_may_drive(from, DrivePlane::Sites) {
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
            && route_sinks_on(&r.route, &me)
            && pubkey_part(r.peer.as_str()) == pubkey_part(sender)
    }

    /// Classify inbound screen/camera media without conflating the normal
    /// Offer→Accept gap with an orphan route. A destructive NACK is correct for
    /// a dead/foreign route, but not for an authenticated same-id re-offer whose
    /// media beat arrived a few milliseconds before its Accept.
    fn inbound_video_disposition(&self, route_id: &str, sender: &str) -> InboundVideoDisposition {
        let Some(me) = self.local_node_id() else {
            return InboundVideoDisposition::Reject;
        };
        let st = self.state.lock();
        let route = st.session.as_ref().and_then(|s| s.route(route_id));
        inbound_video_disposition_from_facts(
            route.map(|r| &r.state),
            route.is_some_and(|r| matches!(r.route.media, MediaKind::Display | MediaKind::Video)),
            route.is_some_and(|r| route_sinks_on(&r.route, &me)),
            route.is_some_and(|r| pubkey_part(r.peer.as_str()) == pubkey_part(sender)),
        )
    }

    /// [`Self::inbound_media_ok`] for the frame kinds two media share:
    /// video frames (and their `vstat` reports) belong to a display route
    /// *or* a camera one — same pipeline, different lens.
    fn inbound_video_ok(&self, route_id: &str, sender: &str) -> bool {
        self.inbound_video_disposition(route_id, sender) == InboundVideoDisposition::Accept
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
                route_sinks_on(&r.route, &me),
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
    /// This is the **owner/fleet** trust only. A person-to-person *share* is the
    /// other authorized path (the owner deliberately granting one plane to
    /// someone outside their fleet); it's honoured per-plane in
    /// [`Self::sender_may_drive`], never here, so a screen-share grant can't
    /// leak into the planes it didn't name.
    fn sender_may_control(&self, sender: &str) -> bool {
        let canon = pubkey_part(sender);
        // You always control your own machine. A loopback terminal/console to
        // the box you're sitting at must pass even when it's unclaimed (no
        // owner) and in no fleet — otherwise opening a terminal to *this*
        // machine on a fresh install is refused, because the owner/fleet roster
        // is empty. `sender` is the authenticated mesh identity, so only a
        // genuine self-route (this node's own id) can match here.
        if let Some(me) = self.local_node_id() {
            if pubkey_part(&me) == canon {
                return true;
            }
        }
        if self.ownership.owner().as_deref().map(pubkey_part) == Some(canon) {
            return true;
        }
        // The owner's own admit records are as authenticated as the signed
        // roster: this device wrote them itself when it admitted (or claimed)
        // the member — local state, never gossip, and already what
        // `in_my_fleet` trusts when deciding evictions. Consulting them here
        // keeps a member controlling its owner's machine working across the
        // window where the daemon's converged roster is still healing (or
        // briefly lost the member to a stale tombstone) — the gap that
        // surfaced as "video streams but keyboard/mouse are refused".
        if self.ownership.any_fleet_member(|d| pubkey_part(d) == canon) {
            return true;
        }
        self.fleet_authorized.lock().contains(canon)
    }

    /// Whether `sender` may drive one privileged `plane` on this machine, at
    /// **admission grade** — evaluating a CEC technician's *live* consent grant.
    /// Used where a route is first authorized (the offer gate) and by the consent
    /// sweep; the per-frame input path uses [`Self::sender_may_drive_admitted`]
    /// instead. See [`Self::may_drive`] for the full owner/fleet/share/CEC rules.
    fn sender_may_drive(&self, sender: &str, plane: DrivePlane) -> bool {
        self.may_drive(sender, plane, true)
    }

    /// The per-frame twin of [`Self::sender_may_drive`]: identical owner/fleet
    /// and person-share checks, but it does **not** re-evaluate a CEC consent
    /// grant. A CEC route is authorized once at admission (the offer gate calls
    /// [`Self::sender_may_drive`]) and torn down within a couple of seconds of
    /// its grant lapsing by [`Self::spawn_cec_consent_sweep`] — so a *live* CEC
    /// route from a known technician is authorized by construction, and the
    /// input hot path (tens of frames a second) must not pay the grant + expiry
    /// evaluation on every one. Owner/fleet and share revocations are *not*
    /// swept, so those stay evaluated here per frame, unchanged.
    fn sender_may_drive_admitted(&self, sender: &str, plane: DrivePlane) -> bool {
        self.may_drive(sender, plane, false)
    }

    /// Whether `sender` may drive one privileged `plane` on this machine: the
    /// owner/fleet trust of [`Self::sender_may_control`], **or** an explicit
    /// person-to-person *share grant* this machine extended that names exactly
    /// that plane. Honouring the grant is what makes a share actually work — the
    /// route authorization already lets a granted route activate, so without
    /// this the console's terminal/files/control/clipboard frames would reach an
    /// active route and then be dropped here ("appears to work but doesn't pass
    /// through"). A grant authorizes only its own plane — a control grant never
    /// opens a shell, a files grant never injects — and the owner/fleet check
    /// runs first, so this only ever *widens* access to exactly who the owner
    /// chose, never narrows the existing owner/fleet path. Config writes
    /// (`SetExposed`) and the `Upgrade` command deliberately stay
    /// owner/fleet-only and keep calling [`Self::sender_may_control`] directly.
    ///
    /// `eval_cec_grant` selects the CEC arm: `true` evaluates the customer's
    /// live consent grant (admission / sweep); `false` trusts an already-admitted
    /// route from a known technician (the per-frame path — see the two wrappers
    /// above). Every drive plane (input, terminal, files, sites, clipboard) maps
    /// to the `Control` capability; screen *viewing* is gated separately at the
    /// Display offer and by the sweep.
    fn may_drive(&self, sender: &str, plane: DrivePlane, eval_cec_grant: bool) -> bool {
        if self.sender_may_control(sender) {
            return true;
        }
        // CEC Support: a dialed technician holds no fleet membership, so the
        // owner/fleet check above fails for them. Their authority is the
        // customer's consent grant — evaluated live at admission and by the
        // ~2s sweep, or (per frame) trusted via the admitted route: a still-live
        // route from a `knows_technician` peer was admitted under a valid grant
        // and has not been swept, so it need not re-hit the store per frame.
        // It only ever *widens* access (the owner/fleet path already said no).
        let cec_ok = if eval_cec_grant {
            self.cec
                .is_allowed(sender, allmystuff_cec_consent::Capability::Control)
        } else {
            self.cec.knows_technician(sender)
        };
        if cec_ok {
            return true;
        }
        let Some(person) = self.shares.person_for_node(pubkey_part(sender)) else {
            return false;
        };
        self.shares
            .out_grants_for(&person.id)
            .iter()
            .any(|g| grant_authorizes_plane(g, plane))
    }

    /// Whether a **screen-viewing** (`Display`/`Video`) offer from `sender` is
    /// authorized under CEC. Refused only when this node is on the customer
    /// side (never dialed anyone) *and* `sender` is a CEC technician it knows
    /// but hasn't granted screen view — the one case a screen offer must be
    /// blocked. Everything else (an ordinary AllMyStuff screen share, a
    /// technician's own node) falls through to the normal path. This is the
    /// screen twin of the per-frame `Control` gate above: a revoke closes it
    /// the next time an offer (or re-offer) is screened. Customer-ness is
    /// role-derived now — with standing area membership there is no hosting
    /// toggle to key on.
    fn cec_screen_offer_denied(&self, sender: &str) -> bool {
        !self.cec.is_technician()
            && self.cec.knows_technician(sender)
            && !self
                .cec
                .is_allowed(sender, allmystuff_cec_consent::Capability::ScreenView)
    }

    /// Whether `sender` may make THIS machine *source* (capture and stream) the
    /// media `route` asks for — its screen, camera, or microphone. This is the
    /// capture-side twin of [`Self::sender_may_drive`]: [`route_drive_plane`]
    /// only ever classified the drive planes (input/terminal/files/sites/
    /// clipboard), so a `Display`/`Video`/`Audio` *source* offer sailed past the
    /// offer gate and reached `start_media` with no authorization at all — the
    /// gap that let any authenticated peer pull this device's screen, webcam, or
    /// mic. Owner and fleet may source anything; a dialed CEC technician's live
    /// `ScreenView` consent covers the screen kinds (`Display`/`Video`; audio is
    /// not part of the CEC consent model); otherwise it takes an explicit
    /// person-to-person share this device extended that lets that person
    /// *consume* this media on this capability (the "Receive your screen"
    /// grant). Mirrors the owner/fleet → CEC → share layering of
    /// [`Self::may_drive`]; screen viewing stays gated here at the offer (and
    /// torn down by [`Self::spawn_cec_consent_sweep`]), never per frame.
    fn sender_may_source_media(&self, sender: &str, route: &Route) -> bool {
        if self.sender_may_control(sender) {
            return true;
        }
        // A dialed CEC technician holds no fleet membership; their authority to
        // view the screen is the customer's live ScreenView consent grant.
        if matches!(route.media, MediaKind::Display | MediaKind::Video)
            && self
                .cec
                .is_allowed(sender, allmystuff_cec_consent::Capability::ScreenView)
        {
            return true;
        }
        // An explicit share this device extended letting that person consume
        // exactly this media on this capability (honours capability pinning and
        // the media/role scope via the canonical `Grant::permits`).
        let Some(person) = self.shares.person_for_node(pubkey_part(sender)) else {
            return false;
        };
        self.shares.out_grants_for(&person.id).iter().any(|g| {
            g.permits(
                route.media,
                allmystuff_graph::GrantRole::Consume,
                &route.from,
            )
        })
    }

    /// Media keeps arriving for a route this side doesn't hold live — our
    /// app restarted (fresh session, old routes gone), or the route tore
    /// down here while the sender missed it. Tell the sender, rate-limited:
    /// its session marks the route rejected and **stops the encoder**
    /// (`Reject` on an active route now returns `StopMedia`), instead of
    /// capturing + encoding into the void indefinitely. An older sender
    /// ignores a Reject for an active route — exactly today's behaviour.
    fn nack_dead_route(self: &Arc<Self>, from: &str, route_id: &str) {
        if !self.diag_ok(&format!("nack:{route_id}")) {
            return;
        }
        let mesh = self.clone();
        let from = from.to_string();
        let route_id = route_id.to_string();
        crate::spawn(async move {
            let _ = mesh
                .send_control(
                    &from,
                    &ControlMessage::Route(RouteControl::Reject {
                        route_id,
                        reason: "route not live on the receiving side — re-offer to reconnect"
                            .into(),
                    }),
                )
                .await;
        });
    }

    /// The lane-shaped twin of [`Self::nack_dead_route`], for the one case
    /// a Reject can't reach: media keeps arriving on a track lane no route
    /// here maps to. That's this app restarted (fresh session — same daemon,
    /// same boot id, so the peer-restart reap never fires) or an orphan
    /// stream shadowing a lane after its route was lost one-sided. We can't
    /// name the dead route — the name is exactly what we lost — but the
    /// sender's own pin still knows, so we report the *lane*
    /// ([`RouteControl::DeadLane`]) and the sender resolves it into a
    /// Reject of that route, stopping its encoder.
    ///
    /// Guarded twice: nothing is sent until the lane has stayed unmapped a
    /// full [`WARN_EVERY`] (a stream's first samples can legally outrun the
    /// Accept/VideoLane control messages at start — a NACK there would kill
    /// a healthy stream being born; [`Self::clear_dead_lane`] wipes the
    /// clock the moment the lane resolves), then rate-limited like every
    /// other diagnostic while the condition persists. An older sender
    /// doesn't know the message and drops it — it keeps streaming exactly
    /// as today.
    fn nack_dead_lane(self: &Arc<Self>, from: &str, media: &'static str, lane: u8) {
        let key = format!("deadlane:{media}:{}:{lane}", pubkey_part(from));
        {
            let mut since = self.dead_lane_since.lock();
            let now = std::time::Instant::now();
            let first = *since.entry(key.clone()).or_insert(now);
            if now.duration_since(first) < WARN_EVERY {
                return;
            }
        }
        if !self.diag_ok(&key) {
            return;
        }
        tracing::warn!(
            "asking {} to stop its unmapped {media} stream on lane {lane} (no route here maps to it)",
            short_id(from)
        );
        let mesh = self.clone();
        let from = from.to_string();
        crate::spawn(async move {
            let _ = mesh
                .send_control(
                    &from,
                    &ControlMessage::Route(RouteControl::DeadLane {
                        media: media.into(),
                        lane,
                    }),
                )
                .await;
        });
    }

    /// The lane resolved to a route again — forget its "unmapped since"
    /// mark so a later unmapped spell starts a fresh [`WARN_EVERY`] grace
    /// instead of inheriting an old clock and NACKing instantly.
    fn clear_dead_lane(&self, from: &str, media: &str, lane: u8) {
        let key = format!("deadlane:{media}:{}:{lane}", pubkey_part(from));
        self.dead_lane_since.lock().remove(&key);
    }

    /// A receiver told us media we're sending it on track `lane` has no
    /// route on its side ([`RouteControl::DeadLane`]) — it can't name the
    /// route (its app restarted; the name is what it lost), but our own
    /// bookkeeping still can. Resolve the lane back to the route we're
    /// streaming *to that peer* — video by the lane pin
    /// ([`Self::assign_video_lane`]'s table), audio by the same positional
    /// sort the outbound forwarder picks lanes with — and fold it through
    /// the session as if the peer had rejected the route by name: the
    /// session re-checks the sender is the route's peer (a spoofed or stale
    /// lane can never kill someone else's stream) and `Reject` on an active
    /// outbound route returns `StopMedia`, which stops the capture that was
    /// encoding into the void. Resolving nothing is a quiet no-op — the
    /// stream already stopped, or an earlier NACK already landed.
    async fn handle_dead_lane(self: &Arc<Self>, from: &str, media: &str, lane: u8) {
        let canon = pubkey_part(from).to_string();
        let route_id = match media {
            "video" => {
                // The pin table is route→lane across all peers; two peers can
                // each hold this lane number, so match the lane and then the
                // peer (via the session, after dropping the pin lock).
                let candidates: Vec<String> = {
                    let pins = self.video_lane_pins.lock();
                    pins.iter()
                        .filter(|(_, l)| **l == lane)
                        .map(|(r, _)| r.clone())
                        .collect()
                };
                candidates.into_iter().find(|rid| {
                    let st = self.state.lock();
                    st.session
                        .as_ref()
                        .and_then(|s| s.route(rid))
                        .is_some_and(|r| pubkey_part(r.peer.as_str()) == canon)
                })
            }
            "audio" => self
                .sorted_media_routes(from, true, "opus")
                .into_iter()
                .nth(lane as usize),
            // A media kind a newer build introduced — nothing of ours to
            // stop; ignore it exactly like an Unknown control message.
            _ => None,
        };
        let Some(route_id) = route_id else {
            tracing::debug!(
                "dead-lane nack from {} for {media} lane {lane} matched no route here",
                short_id(from)
            );
            return;
        };
        tracing::warn!(
            "receiver {} reports our {media} on lane {lane} maps to no route on its side — \
             stopping {route_id}",
            short_id(from)
        );
        let effects = {
            let mut st = self.state.lock();
            st.session
                .as_mut()
                .map(|s| {
                    s.handle(
                        NodeId::from(from),
                        ControlMessage::Route(RouteControl::Reject {
                            route_id,
                            reason: "no route on the receiving side maps to this stream's lane \
                                     — re-offer to reconnect"
                                .into(),
                        }),
                    )
                })
                .unwrap_or_default()
        };
        self.process_effects(effects).await;
        self.emit_snapshot();
    }

    /// An inbound input/clipboard frame failed a gate. Historically this was
    /// one rate-unlimited, cause-blind warn — which is exactly how "the mouse
    /// stopped working" became undiagnosable: the viewer's console looked
    /// connected (the route activates regardless) while every event died
    /// here. Now, rate-limited per route: log *which* gate failed with the
    /// route facts, surface it on this machine's own UI sink, and send a
    /// `RouteControl::Reject` back so the viewer's console flips its toggle
    /// off and shows the reason instead of typing into the void. (An old
    /// viewer ignores a Reject for an active route — no worse than today.)
    fn refuse_control_frame(
        self: &Arc<Self>,
        from: &str,
        route_id: &str,
        plane: &str,
        route_ok: bool,
    ) {
        if !self.diag_ok(&format!("refuse:{plane}:{route_id}")) {
            return;
        }
        let reason = if route_ok {
            if self.cec.knows_technician(from) {
                // A CEC technician's authority is the customer's consent grant,
                // not the fleet roster — when their frames die here it's the
                // grant that lapsed (expired, revoked, or an "Approve Once"
                // lost to an app restart). Say that, or the technician goes
                // hunting through fleet settings that were never involved.
                format!(
                    "{plane} refused: the customer's approval no longer covers it \
                     (expired, revoked, or their app restarted) — reconnect so they \
                     can approve again"
                )
            } else {
                format!(
                    "{plane} refused: this machine doesn't recognize the controlling device as \
                     its owner or a fleet member (and no {plane} share covers it) — check the \
                     fleet roster / re-admit the device from Fleet settings"
                )
            }
        } else {
            format!(
                "{plane} refused: no live {plane} route for it here ({}) — reconnect the console",
                self.route_diag(route_id, from)
            )
        };
        tracing::warn!("dropped {plane} event from {}: {reason}", short_id(from));
        self.sink.emit(
            "allmystuff://control-refused",
            serde_json::json!({
                "route": route_id,
                "from": from,
                "plane": plane,
                "reason": reason,
            }),
        );
        let mesh = self.clone();
        let from = from.to_string();
        let route_id = route_id.to_string();
        crate::spawn(async move {
            let _ = mesh
                .send_control(
                    &from,
                    &ControlMessage::Route(RouteControl::Reject { route_id, reason }),
                )
                .await;
        });
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
            // 300 ms floor: a re-key is the recovery from visible corruption, so
            // it must turn around fast (was 600 ms). Still throttled so a viewer
            // failing every frame can't trigger a keyframe storm — at most a few
            // re-keys/s while it's actually broken.
            if asks
                .get(&route_id)
                .is_some_and(|t| now.duration_since(*t) < std::time::Duration::from_millis(300))
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
    /// GUI-internal: the effective encode dials for a route THIS node is
    /// streaming — the "what we're actually doing" half of the console's
    /// quality panel (resolved posture, encoder rung, wire codec, the AIMD
    /// bitrate target + its ceiling, the fps + edge targets, and the actual
    /// output geometry). `None` when this node isn't the streamer for
    /// `route_id` (the ordinary remote-view case, where the viewer surfaces
    /// its own measured actuals). Read-only; touches no wire and no peer.
    pub fn route_dials(&self, route_id: &str) -> Option<crate::video::RouteDials> {
        self.video.route_dials(route_id)
    }

    pub async fn request_tune(
        self: &Arc<Self>,
        route_id: String,
        max_edge: Option<u32>,
        bitrate: Option<u32>,
        fps: Option<u32>,
        game: bool,
        mode: Option<String>,
    ) -> Result<(), String> {
        let peer = self.route_peer(&route_id).ok_or("unknown route")?;
        // The streaming side logs the retune it actually applies — one
        // line per pill change across the pair is plenty.
        tracing::debug!(
            "asking {} to tune {route_id}: edge {max_edge:?} · bitrate {bitrate:?} · fps {fps:?} · game {game} · mode {mode:?}",
            short_id(&peer)
        );
        self.send_control(
            &peer,
            &ControlMessage::Route(RouteControl::Tune {
                route_id,
                max_edge,
                bitrate,
                fps,
                game,
                mode,
                // No viewer-requested pipeline knob today; the seam is here
                // for when one lands (backend-only, no wire change).
                ext: serde_json::Value::Null,
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
        lost_ts_us: Option<u64>,
    ) -> Result<(), String> {
        let peer = self.route_peer(&route_id).ok_or("unknown route")?;
        // Enrich with what this end measured about the link itself: the
        // chunk-train bandwidth estimate + delay trend (T1.1). Rides the
        // same control channel on the ICE datapath as the report always
        // did — zeros for routes with no timed trains yet.
        let (est_kbps, delay_trend_us_per_s) = self.route_link_estimate(&route_id);
        let ext = crate::video::PipelineFeedback {
            est_kbps,
            delay_trend_us_per_s,
        }
        .to_ext();
        self.send_control(
            &peer,
            &ControlMessage::Route(RouteControl::VideoFeedback {
                route_id,
                recv_fps,
                decode_fails,
                queue_depth,
                lost_ts_us,
                ext,
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
                && route_sources_on(&r.route, &me))
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
                && route_sources_on(&r.route, &me))
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
                && route_sources_on(&r.route, &me))
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
                route_sinks_on(&r.route, &me),
                route_sources_on(&r.route, &me),
            )
        };

        if sinks_here {
            if !self.sender_may_drive(from, DrivePlane::Clipboard) {
                // Same loud refusal as input: the route was live (checked
                // above), so the failed gate is authorization.
                self.refuse_control_frame(from, &frame.route, "clipboard", true);
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

    /// Ordered networks to try when sending to `peer`: the last network the
    /// slot proved (an inbound frame, or a prior confirmed send), then the
    /// primary network, then every other joined network. A multi-homed peer —
    /// a KVM sits on its fleet mesh, the local-claim mesh and the CEC help
    /// mesh at once, broadcasting presence on all of them — keeps overwriting
    /// the single `peer_networks` slot with whichever mesh delivered its last
    /// advert, and that mesh is not necessarily one that carries OUR frames
    /// back (hub topologies relay a beacon without giving us a direct lane).
    /// One slot, one attempt was the lottery behind "shows up online, the
    /// site opens, but nothing connects."
    fn peer_network_candidates(&self, peer: &str) -> Vec<String> {
        let st = self.state.lock();
        ordered_send_candidates(
            st.peer_networks.get(pubkey_part(peer)),
            st.network.as_ref(),
            &st.networks,
        )
    }

    /// Record that a send to `peer` was daemon-confirmed on `network`, so the
    /// tunnel traffic that follows (site/input frames ride the slot) sticks to
    /// a mesh that provably reaches the peer — until the next inbound frame or
    /// confirmed send updates it again.
    fn note_peer_network(&self, peer: &str, network: &str) {
        let mut st = self.state.lock();
        let key = pubkey_part(peer).to_string();
        if st.peer_networks.get(&key).map(String::as_str) != Some(network) {
            st.peer_networks.insert(key, network.to_string());
        }
    }

    /// Send a control message to one peer, reporting whether the daemon
    /// actually dispatched it. The daemon's peer set is keyed by the *bare
    /// pubkey* (what signaling announces), while AllMyStuff mostly holds
    /// display ids (`pubkey-SUFFIX`, what presence and `IdentityShow` carry)
    /// — so the id is canonicalised here, at the daemon boundary. Addressing
    /// the display form made every send come back "peer not found", an error
    /// this used to swallow: a claim showed "asking…" and then nothing.
    ///
    /// Tries every shared network until the daemon confirms one (the KVM's
    /// bridge sweeps its networks the same way — "the correct network's send
    /// reaches them and others are harmless no-ops"), then pins the peer's
    /// slot to the network that actually delivered, so the media frames that
    /// follow a route offer ride the proven mesh instead of the last one a
    /// presence advert happened to arrive on.
    async fn send_control(&self, peer: &str, message: &ControlMessage) -> Result<(), String> {
        let candidates = self.peer_network_candidates(peer);
        if candidates.is_empty() {
            return Err(format!("no shared network with {peer}"));
        }
        let payload = serde_json::to_value(message).map_err(|e| e.to_string())?;
        let mut last_err = String::new();
        for network in candidates {
            let resp = self
                .client
                .request(&Request::ChannelSendTo {
                    network: network.clone(),
                    channel: CHANNEL_CONTROL.to_string(),
                    peer: pubkey_part(peer).to_string(),
                    payload: payload.clone(),
                })
                .await;
            match resp {
                Ok(r) if r.ok => {
                    self.note_peer_network(peer, &network);
                    return Ok(());
                }
                Ok(r) => {
                    last_err = r.error.unwrap_or_else(|| "channel send failed".into());
                }
                Err(e) => last_err = e.to_string(),
            }
        }
        tracing::warn!("control send to {peer} failed on every shared network: {last_err}");
        Err(last_err)
    }

    fn emit_snapshot(&self) {
        self.sink.emit("allmystuff://session", self.snapshot());
    }

    fn emit_status(&self, status: &str, error: Option<&str>) {
        // Remember it: the emit is fire-and-forget (a GUI that subscribed
        // late never hears it), so `mesh_status` answers with this instead
        // of the front-end inferring liveness from unrelated calls.
        *self.last_status.lock() = (status.to_string(), error.map(str::to_string));
        self.sink.emit(
            "allmystuff://subscription",
            json!({ "status": status, "error": error }),
        );
    }

    /// The daemon-link status as last emitted on `allmystuff://subscription`
    /// (`live` / `no_network` / `disconnected`, plus the error that caused
    /// it) — the front-end's poll-safe way to learn the *current* state
    /// instead of hoping it caught a one-shot event.
    pub fn link_status(&self) -> (String, Option<String>) {
        self.last_status.lock().clone()
    }
}

/// A well-formed but empty owned roster (no fleet yet).
fn empty_owned() -> Value {
    json!({ "key": "", "version": 0, "members": [], "is_owner": false, "network_id": "" })
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

/// Whether two node ids name the **same machine**, ignoring the display
/// suffix ([`pubkey_part`] strips the `-<5char>` the UI appends). Routing and
/// presence carry the bare node id, while the front-end builds a route's
/// capability ids from the suffixed display id — so a self / loopback check
/// (`is this route to my own machine?`) must compare canonically. A raw `==`
/// misses a genuine self-route when the two forms differ and tries to send a
/// local terminal out over the wire, where it never comes back.
fn same_node(a: &str, b: &str) -> bool {
    pubkey_part(a) == pubkey_part(b)
}

fn route_sources_on(route: &Route, node: &str) -> bool {
    same_node(&node_of(route.from.as_str()), node)
}

fn route_sinks_on(route: &Route, node: &str) -> bool {
    same_node(&node_of(route.to.as_str()), node)
}

/// The RTP video lane to pin a new route to `peer_canon` on: its existing pin
/// if it already has one, else the **lowest lane in `[0, cap)` not already
/// taken** by another of that peer's pinned routes. `None` only when the pool
/// is full. Pure (takes the pin map directly) so the race-free assignment is
/// unit-tested. A pinned route's peer is the `to` node of its id
/// (`route:<from>→<to>`); pins for other peers don't constrain this one.
fn free_lane_for_peer(
    pins: &std::collections::HashMap<String, u8>,
    peer_canon: &str,
    route_id: &str,
    cap: u8,
) -> Option<u8> {
    if let Some(&lane) = pins.get(route_id) {
        return Some(lane);
    }
    let used: std::collections::HashSet<u8> = pins
        .iter()
        .filter(|(rid, _)| {
            rid.as_str() != route_id
                && rid
                    .split_once('→')
                    .is_some_and(|(_, to)| pubkey_part(&node_of(to)) == peer_canon)
        })
        .map(|(_, &l)| l)
        .collect();
    (0..cap).find(|l| !used.contains(l))
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

// ---- refresh round-trip backoff -------------------------------------------
//
// The per-node refresh asks a peer to re-announce its profile
// ([`ControlMessage::ProfileRequest`]). To keep a held-down refresh from
// hammering a peer, the asker spaces those requests per target under a growing
// envelope: at most one every `PROFILE_REQ_MIN_SECS`, and that floor *doubles*
// each minute of a sustained burst up to a `PROFILE_REQ_MAX_SECS` ceiling
// (5 → 10 → 20 → 40 → 60 s). The envelope resets to its fast floor after a
// `PROFILE_REQ_RESET_IDLE` quiet spell, or after it's sat at the ceiling for
// `PROFILE_REQ_CAP_HOLD` (so a steady once-a-minute refresh eventually earns a
// fresh fast window).

/// Floor between refresh round-trips to one peer — "at most every 5 s".
const PROFILE_REQ_MIN_SECS: u64 = 5;
/// Ceiling the floor grows to over a sustained burst — "down to once a minute".
const PROFILE_REQ_MAX_SECS: u64 = 60;
/// Quiet spell after which the envelope resets to its fast floor.
const PROFILE_REQ_RESET_IDLE: std::time::Duration = std::time::Duration::from_secs(5 * 60);
/// How long the envelope may sit at the ceiling before it resets anyway.
const PROFILE_REQ_CAP_HOLD: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// Per-peer backoff state for the refresh round-trip.
#[derive(Clone, Copy)]
struct ProfileReqState {
    /// When the current burst of refreshes began (drives the growing floor).
    burst_start: std::time::Instant,
    /// When we last actually sent a request.
    last_request: std::time::Instant,
}

/// The minimum spacing between refresh round-trips given how long the current
/// burst has run: `PROFILE_REQ_MIN_SECS` through the first minute, then doubling
/// each further minute (10, 20, 40 s …) up to the `PROFILE_REQ_MAX_SECS` ceiling.
fn profile_req_interval(burst_age: std::time::Duration) -> std::time::Duration {
    let level = (burst_age.as_secs() / 60).min(64) as u32; // burst minute (0-based)
    let secs = PROFILE_REQ_MIN_SECS
        .checked_shl(level)
        .unwrap_or(PROFILE_REQ_MAX_SECS)
        .min(PROFILE_REQ_MAX_SECS);
    std::time::Duration::from_secs(secs)
}

/// The burst age at which the floor first reaches the ceiling — where the
/// "sat at the cap" reset window starts counting from.
fn profile_req_cap_reached() -> std::time::Duration {
    let mut level = 0u32;
    while PROFILE_REQ_MIN_SECS.checked_shl(level).unwrap_or(u64::MAX) < PROFILE_REQ_MAX_SECS {
        level += 1;
    }
    std::time::Duration::from_secs(u64::from(level) * 60)
}

/// The pure backoff decision (factored out so the envelope is unit-testable
/// without a clock): given the prior per-peer state and `now`, whether a
/// refresh round-trip is allowed, and the state to store. Resets the burst
/// after a long idle or a long hold at the ceiling.
fn profile_req_decide(
    prev: Option<ProfileReqState>,
    now: std::time::Instant,
) -> (bool, ProfileReqState) {
    let Some(mut st) = prev else {
        return (
            true,
            ProfileReqState {
                burst_start: now,
                last_request: now,
            },
        );
    };
    let idle = now.duration_since(st.last_request);
    if idle >= PROFILE_REQ_RESET_IDLE
        || now.duration_since(st.burst_start) >= profile_req_cap_reached() + PROFILE_REQ_CAP_HOLD
    {
        st.burst_start = now;
    }
    let interval = profile_req_interval(now.duration_since(st.burst_start));
    if now.duration_since(st.last_request) >= interval {
        st.last_request = now;
        (true, st)
    } else {
        (false, st)
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

/// An explicit drive mapping: one physical Storage capability is offered to
/// the other machine's synthetic `:storage-in` sink. Unlike `:files`, this is
/// scoped to that one mounted volume and the active route is the lease.
fn is_mapped_drive_route(route: &Route) -> bool {
    route.media == MediaKind::Storage
        && route.to.as_str().ends_with(":storage-in")
        && !route.from.as_str().ends_with(":storage-in")
}

/// A local drive push can start for an owned/fleet target, or for a customer
/// this technician actually dialed while that exact support session is active.
/// Keeping both CEC facts prevents a stale directory entry (or an unrelated
/// active session) from widening native drive mapping.
fn drive_push_may_start(owner_or_fleet: bool, dialed_customer: bool, active_support: bool) -> bool {
    owner_or_fleet || (dialed_customer && active_support)
}

/// Admission for the mapped-drive offer at the destination. Receiver-minted
/// pulls are already bound to a one-use token; unsolicited pushes need the
/// Files plane, which includes owner/fleet, an explicit Files share, and a live
/// CEC Control grant.
fn mapped_drive_offer_authorized(explicit_pull: bool, files_plane: bool) -> bool {
    explicit_pull || files_plane
}

/// Resolve the route's source capability against a fresh local inventory.
/// The peer never supplies a root: it must exactly name a currently mounted
/// volume this node advertised, and only its recorded mount point is retained.
fn mapped_drive_root(route: &Route, me: &str) -> Option<std::path::PathBuf> {
    if !is_mapped_drive_route(route) || !route_sources_on(route, me) {
        return None;
    }
    let prefix = format!("{}:", node_of(route.from.as_str()));
    let local_cap = route.from.as_str().strip_prefix(&prefix)?;
    allmystuff_inventory::scan()
        .storage
        .into_iter()
        .find(|volume| volume.id == local_cap)
        .and_then(|volume| volume.mount_point)
        .map(std::path::PathBuf::from)
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

/// A privileged plane a peer can drive on this machine — the unit a share
/// grant authorizes. Owner/fleet trust covers every plane; a person-to-person
/// share covers only the exact plane(s) the owner granted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DrivePlane {
    /// Keyboard/mouse injection into this machine's `:control` input sink.
    Input,
    /// A shell on this machine.
    Terminal,
    /// This machine's disk.
    Files,
    /// Reverse-proxying a service this machine exposes.
    Sites,
    /// Writing this machine's clipboard (rides with the control grant).
    Clipboard,
}

/// The privileged plane a route carries, if any — so the offer screen and the
/// per-frame gate authorize the same plane for the same route.
fn route_drive_plane(route: &Route) -> Option<DrivePlane> {
    if is_terminal_route(route) {
        Some(DrivePlane::Terminal)
    } else if is_files_route(route) {
        Some(DrivePlane::Files)
    } else if is_mapped_drive_route(route) {
        // This matters at offer admission: a peer cannot fabricate a route
        // that makes this machine source one of its disks. A locally-created
        // mapping is already active on the source side and does not pass
        // through the inbound-offer gate there.
        Some(DrivePlane::Files)
    } else if is_site_route(route) {
        Some(DrivePlane::Sites)
    } else {
        None
    }
}

/// Whether `grant` authorizes `plane`. Each plane maps to exactly the grant the
/// share builder mints for it (`gui/src/store.svelte.ts::shareCapGrants`), so
/// the planes never cross-authorize: a control (input) grant only injects, a
/// files (storage) grant only reaches the disk, terminal/sites are distinct
/// generic grants told apart by their capability suffix.
fn grant_authorizes_plane(grant: &Grant, plane: DrivePlane) -> bool {
    let cap_ends = |suffix: &str| {
        grant
            .capability
            .as_ref()
            .is_some_and(|c| c.as_str().ends_with(suffix))
    };
    match plane {
        DrivePlane::Input => grant.media == MediaKind::Input && grant.role.allows_sink(),
        DrivePlane::Terminal => grant.media == MediaKind::Generic && cap_ends(":terminal"),
        DrivePlane::Files => grant.media == MediaKind::Storage && cap_ends(":files"),
        DrivePlane::Sites => grant.media == MediaKind::Generic && cap_ends(":sites"),
        DrivePlane::Clipboard => grant.media == MediaKind::Clipboard,
    }
}

/// |skew| at which the passive clock estimate warns (10 s: far beyond
/// presence-delivery jitter, well inside the range where wall-clock
/// last-writer-wins and TOTP windows start misbehaving).
const CLOCK_SKEW_WARN_MS: i64 = 10_000;
/// |skew| the estimate must fall back under before a raised warning clears —
/// hysteresis so the warning doesn't flap at the threshold.
const CLOCK_SKEW_CLEAR_MS: i64 = 5_000;

/// Median of `samples` (odd length), or the **smaller-magnitude** middle
/// (even length). The conservative even-length pick means a *strict
/// majority* of peers must agree we're off before the network estimate
/// crosses a threshold: two peers split [0 s, 60 s] verdicts 0 — it's that
/// peer's clock that's wrong, and its own node warns against *its* peers.
fn conservative_median(samples: &[i64]) -> Option<i64> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let n = sorted.len();
    if n % 2 == 1 {
        return Some(sorted[n / 2]);
    }
    let (a, b) = (sorted[n / 2 - 1], sorted[n / 2]);
    Some(if a.abs() <= b.abs() { a } else { b })
}

/// This machine's wall clock as Unix-epoch milliseconds — the presence
/// `sent_at` stamp.
fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Whether a fleet member's presence advert is evidence it left the fleet:
/// only an owner it *positively names* that isn't us. `None` (no owner in the
/// advert) is ambiguous — an early advert sent before its ownership store
/// loaded, an older build, a foreign bridge — and must never author the
/// eviction tombstone that roster convergence then propagates fleet-wide.
/// Pure, because getting this wrong is how remote control silently died once.
fn fleet_departure(advertised_owner: Option<&str>, me: Option<&str>) -> bool {
    match advertised_owner {
        Some(owner) => Some(owner) != me,
        None => false,
    }
}

/// Whether CEC consent should block a screen offer after the ordinary
/// AllMyStuff authority has been evaluated. A peer can be both a known CEC
/// technician and an owner/fleet member (for example, the same laptop runs
/// CECSupport and AllMyStuff). CEC is an additional authorization path; it
/// must never narrow owner/fleet or explicit-share access merely because that
/// identity also appears in the support directory.
fn cec_screen_consent_blocks(media_authorized: bool, cec_consent_denied: bool) -> bool {
    cec_consent_denied && !media_authorized
}

/// Why an inbound terminal/files/site offer must be refused, if it must: it
/// asks *this* machine to host a shell (or hand over its disk, or proxy a
/// service) and the offerer is neither owner/fleet nor holds a share grant for
/// that plane (`authorized` folds both — the caller computes it per the route's
/// plane). `None` = fine (not a privileged offer, not our side to host, or the
/// sender is authorized). Pure, so the rule that guards the most privileged
/// things on the mesh is unit-testable.
fn privileged_offer_refusal(route: &Route, hosts_here: bool, authorized: bool) -> Option<String> {
    if !hosts_here || authorized {
        return None;
    }
    if is_terminal_route(route) {
        return Some("not authorized: terminal access needs owner/fleet or a share".into());
    }
    if is_files_route(route) {
        return Some("not authorized: file access needs owner/fleet or a share".into());
    }
    if is_mapped_drive_route(route) {
        return Some("not authorized: drive mapping needs owner/fleet or a share".into());
    }
    if is_site_route(route) {
        return Some("not authorized: site access needs owner/fleet or a share".into());
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
/// Date each device the fleet's signed governance has removed: canonical pubkey
/// → the unix second its removal was authored.
///
/// Takes the raw `GovernanceState` reply. `evicted` is the authoritative,
/// already-verified answer to *whether* a device is removed; `state.member_log`
/// is read only for **when**, so nothing here re-decides membership or trusts an
/// unverified entry. The latest entry targeting a removed device IS its removal
/// — that is what made the verdict come out removed under the member log's
/// last-writer-wins fold — so the max stamp over that device's entries is the
/// tombstone's stamp without having to match variant kinds.
///
/// A removed device the log can't date is recorded at `u64::MAX`, which outranks
/// every claim: an undatable eviction keeps the old always-prune behaviour
/// rather than being quietly waived.
fn eviction_stamps(data: &Value) -> std::collections::HashMap<String, u64> {
    let removed: std::collections::HashSet<String> = data
        .get("evicted")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(|k| pubkey_part(k).to_string())
                .collect()
        })
        .unwrap_or_default();
    if removed.is_empty() {
        return std::collections::HashMap::new();
    }

    let mut at: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    if let Some(log) = data
        .get("state")
        .and_then(|s| s.get("member_log"))
        .and_then(|v| v.as_array())
    {
        for entry in log {
            let Some(stamp) = entry.get("at").and_then(|v| v.as_u64()) else {
                continue;
            };
            let Some(target) = entry
                .get("variant")
                .and_then(|v| v.get("target"))
                .and_then(|v| v.as_str())
            else {
                continue;
            };
            let target = pubkey_part(target).to_string();
            if !removed.contains(&target) {
                continue;
            }
            at.entry(target)
                .and_modify(|s| *s = (*s).max(stamp))
                .or_insert(stamp);
        }
    }

    removed
        .into_iter()
        .map(|k| {
            let stamp = at.get(&k).copied().unwrap_or(u64::MAX);
            (k, stamp)
        })
        .collect()
}

fn pubkey_part(id: &str) -> &str {
    if let Some((body, suffix)) = id.rsplit_once('-') {
        if suffix.len() == 5 && suffix.chars().all(|c| c.is_ascii_alphanumeric()) {
            return body;
        }
    }
    id
}

/// Video feedback is generated repeatedly by a window that still owns the
/// route, even for a static screen whose paint rate is zero. One-shot
/// setup/tune controls are intentionally excluded: they can already be in
/// flight beside the stale close we are fencing.
fn inbound_video_feedback_liveness_route_id(msg: &ControlMessage) -> Option<&str> {
    match msg {
        ControlMessage::Route(RouteControl::VideoFeedback { route_id, .. }) => Some(route_id),
        _ => None,
    }
}

fn watcher_poll_proves_liveness(last_poll: Instant, disconnect_started: Instant) -> bool {
    last_poll >= disconnect_started + VIDEO_LOCAL_POLL_PROOF_MIN_AGE
}

/// Fold one network's daemon peer list into the `pubkey → network` map that
/// [`Mesh::network_for_peer`] addresses control/media with. Each peer the daemon
/// reports **reachable** (`active`/`shelved` — the same cut the graph reads
/// "online" from) learns *this* network as where to address it, keyed by
/// canonical pubkey and only when it has no network yet: a mapping already
/// learned from an inbound frame is proven to carry traffic to us and must win,
/// so this only *fills the gap* for a peer the daemon reports connected but that
/// we have not yet heard from directly. Pure (no daemon, no lock) so the
/// reachable-only / gap-fill / canonical-key rules are unit-tested. See
/// [`Mesh::refresh_peer_networks`] for why the gap is what stranded a peer
/// sharing only a secondary mesh.
/// The daemon peer-status values that count as **reachable** — a live link
/// (`active`/`shelved`), the same cut the graph reads "online" from. An
/// offline / sighted / handshaking / errored row is a peer the daemon
/// remembers, not one you can reach right now.
fn status_is_reachable(status: Option<&str>) -> bool {
    matches!(status, Some("active") | Some("shelved"))
}

fn seed_peer_networks(map: &mut HashMap<String, String>, peers: &[Value], network: &str) {
    for p in peers {
        if !status_is_reachable(p.get("status").and_then(|v| v.as_str())) {
            continue;
        }
        if let Some(id) = p.get("device_id").and_then(|v| v.as_str()) {
            map.entry(pubkey_part(id).to_string())
                .or_insert_with(|| network.to_string());
        }
    }
}

/// The try-order for sending to one peer: its slot (last proven network)
/// first, then the primary, then every other joined network, deduped in that
/// priority. Pure, so the order — the part that decides whether a
/// multi-homed peer's tunnel finds its live mesh — is testable on its own;
/// [`Mesh::peer_network_candidates`] feeds it the live state.
fn ordered_send_candidates(
    slot: Option<&String>,
    primary: Option<&String>,
    joined: &[String],
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in slot.into_iter().chain(primary).chain(joined) {
        if !out.contains(n) {
            out.push(n.clone());
        }
    }
    out
}

/// One peer row's link class off the daemon's `selected_pair` — the
/// daemon's own LAN/STUN/TURN rule (host↔host = LAN, which already folds
/// in its private-address override), reduced to the two classes the video
/// gate cares about. No pair reported (ICE unsettled, or a daemon that
/// predates the field) is `Unknown` — the caller must treat that as
/// "don't know", never as a downgrade.
fn link_class_of(peer: &Value) -> crate::video::LinkClass {
    use crate::video::LinkClass;
    let Some(pair) = peer.get("selected_pair").filter(|v| !v.is_null()) else {
        return LinkClass::Unknown;
    };
    let kind = |k: &str| pair.get(k).and_then(|v| v.as_str());
    match (kind("local"), kind("remote")) {
        (Some("host"), Some("host")) => LinkClass::Lan,
        (Some(_), Some(_)) => LinkClass::Wan,
        _ => LinkClass::Unknown,
    }
}

/// Seed `peer_links` from one network's peer list, returning the peers
/// whose class actually CHANGED (Lan↔Wan, or first classification) — the
/// callers retune live streams on those. `Unknown` never touches the map:
/// the daemon clears `selected_pair` on a transient ICE Disconnected, and
/// yanking a stream's dials on a blip would be the gate punishing
/// recovery. Pure (no daemon, no lock), like [`seed_peer_networks`], so
/// the keep-on-unknown rule is unit-tested.
fn seed_peer_links(
    map: &mut HashMap<String, crate::video::LinkClass>,
    peers: &[Value],
) -> Vec<(String, crate::video::LinkClass)> {
    use crate::video::LinkClass;
    let mut changed = Vec::new();
    for p in peers {
        let Some(id) = p.get("device_id").and_then(|v| v.as_str()) else {
            continue;
        };
        let class = link_class_of(p);
        if class == LinkClass::Unknown {
            continue;
        }
        let key = pubkey_part(id).to_string();
        if map.get(&key) != Some(&class) {
            map.insert(key.clone(), class);
            changed.push((key, class));
        }
    }
    changed
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

/// A fresh opaque id for one chat line — 16 random bytes as hex, unique within a
/// session so the receiver can dedupe and the sender can recognise the echo of
/// its own message. Mirrors [`fresh_share_token`]; the `msg_` prefix only tells
/// the two apart in a trace.
/// The `cec://viewing` event / `cec_viewing` command payload: technician
/// canonical id → what their live routes actually carry right now.
fn cec_viewing_value(viewing: &std::collections::BTreeMap<String, (bool, bool)>) -> Value {
    let techs: serde_json::Map<String, Value> = viewing
        .iter()
        .map(|(tech, (screen, control))| {
            (
                tech.clone(),
                json!({ "screen": screen, "control": control }),
            )
        })
        .collect();
    json!({ "techs": techs })
}

fn fresh_chat_id() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::getrandom(&mut bytes).is_err() {
        // RNG unavailable (vanishingly rare): a wall-clock nonce is unique
        // enough for one app run.
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(1);
        return format!("msg_{n:032x}");
    }
    let mut s = String::with_capacity(4 + 32);
    s.push_str("msg_");
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

fn is_viewer_file_request(event: &FileEvent) -> bool {
    matches!(
        event,
        FileEvent::Volumes { .. }
            | FileEvent::List { .. }
            | FileEvent::Read { .. }
            | FileEvent::Stat { .. }
            | FileEvent::ReadRange { .. }
            | FileEvent::Fetch { .. }
            | FileEvent::Write { .. }
            | FileEvent::WriteRange { .. }
            | FileEvent::Mkdir { .. }
            | FileEvent::Rename { .. }
            | FileEvent::Delete { .. }
    )
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
    // ---- eviction stamps -------------------------------------------------
    //
    // The JSON below matches what `GovernanceState` actually emits — verified
    // against myownmesh-core's `Transition`, which serialises the variant as a
    // NESTED object ({"at":..,"variant":{"kind":"evict","target":".."},..}), not
    // flattened. If that ever changes, `eviction_stamps` silently dates nothing,
    // every eviction sorts as u64::MAX and the prune goes back to being
    // unconditional — so these pin the shape as much as the logic.

    #[test]
    fn eviction_stamps_dates_each_removal_from_the_member_log() {
        let data = serde_json::json!({
            "evicted": ["kvmkey"],
            "state": {"member_log": [
                {"at": 100, "variant": {"kind": "role_grant", "target": "kvmkey", "role": "member"},
                 "signers": ["owner"], "signatures": ["s"]},
                {"at": 200, "variant": {"kind": "evict", "target": "kvmkey"},
                 "signers": ["owner"], "signatures": ["s"]},
                // A different device's entries must not leak into the stamp.
                {"at": 900, "variant": {"kind": "evict", "target": "otherkey"},
                 "signers": ["owner"], "signatures": ["s"]},
            ]}
        });

        let stamps = super::eviction_stamps(&data);

        assert_eq!(stamps.len(), 1, "only devices in `evicted` are dated");
        assert_eq!(stamps.get("kvmkey"), Some(&200));
    }

    #[test]
    fn an_undatable_eviction_outranks_every_claim() {
        // No member log to read (an older daemon, a read that lost the field).
        // The device is still removed, so it must stay pruned rather than being
        // waived by a stamp we couldn't find.
        let data = serde_json::json!({"evicted": ["kvmkey"]});

        let stamps = super::eviction_stamps(&data);

        assert_eq!(stamps.get("kvmkey"), Some(&u64::MAX));
    }

    #[test]
    fn no_evictions_dates_nothing() {
        let data = serde_json::json!({"evicted": [], "state": {"member_log": []}});
        assert!(super::eviction_stamps(&data).is_empty());
    }

    #[test]
    fn eviction_stamps_canonicalises_the_display_suffix() {
        // The log and the `evicted` list may carry either the bare pubkey or a
        // display id; the local member list is canonical, so both sides must
        // collapse or the lookup misses and the prune runs anyway.
        let data = serde_json::json!({
            "evicted": ["kvmkey-AB12C"],
            "state": {"member_log": [
                {"at": 500, "variant": {"kind": "evict", "target": "kvmkey"},
                 "signers": ["owner"], "signatures": ["s"]},
            ]}
        });

        let stamps = super::eviction_stamps(&data);

        assert_eq!(stamps.get("kvmkey"), Some(&500));
    }

    use super::*;

    #[test]
    fn volume_inventory_is_a_viewer_file_request() {
        assert!(is_viewer_file_request(&FileEvent::Volumes { req: 7 }));
        assert!(!is_viewer_file_request(&FileEvent::VolumeList {
            req: 7,
            volumes: Vec::new(),
        }));
    }

    #[test]
    fn video_route_generation_fences_same_id_successors() {
        let mut generations = VideoRouteGenerations::default();
        let (first, replaced) = generations.begin("route:display");
        assert_eq!(replaced, None);
        assert!(generations.is_current("route:display", first));

        // A real successor can start before the predecessor's stale StopMedia
        // arrives, so no retire occurs between these calls. It still must mint
        // a new generation and fence every queued predecessor callback.
        let (successor, replaced) = generations.begin("route:display");
        assert_eq!(replaced, Some(first));
        assert_ne!(successor, first);
        assert!(!generations.is_current("route:display", first));
        assert!(generations.is_current("route:display", successor));

        generations.retire("route:display");
        assert!(!generations.is_current("route:display", successor));
        let (third, replaced) = generations.begin("route:display");
        assert_eq!(replaced, None);
        assert_ne!(third, successor);
        assert!(generations.is_current("route:display", third));
    }

    #[test]
    fn monitor_switch_fences_duplicate_early_teardowns() {
        let mut guards = VideoSwitchGuards::default();
        let now = Instant::now();
        guards.note_stop("route:primary", "viewer-ABCDE", "viewer:display:0", now);
        guards.note_start(
            "route:secondary",
            "viewer-ABCDE",
            "viewer:display:0",
            now + Duration::from_millis(8),
        );

        let hit = guards
            .take_early_teardown(
                "route:secondary",
                "viewer-FGHIJ",
                now + Duration::from_millis(15),
            )
            .expect("the first close inside the measured switch race is fenced");
        assert_eq!(hit.predecessor, "route:primary");
        assert_eq!(hit.age, Duration::from_millis(7));
        assert!(
            guards
                .take_early_teardown(
                    "route:secondary",
                    "viewer-ABCDE",
                    now + Duration::from_millis(16),
                )
                .is_some(),
            "a concurrent duplicate cannot consume and defeat the guard"
        );
        assert!(
            guards
                .take_early_teardown(
                    "route:secondary",
                    "viewer-ABCDE",
                    now + Duration::from_millis(8)
                        + VIDEO_SWITCH_TEARDOWN_GUARD
                        + Duration::from_millis(1),
                )
                .is_none(),
            "the guard remains strictly time bounded"
        );
    }

    #[test]
    fn monitor_switch_guard_is_sink_scoped_and_time_bounded() {
        let now = Instant::now();
        let mut guards = VideoSwitchGuards::default();
        guards.note_stop("route:old", "viewer", "viewer:display:0", now);
        guards.note_start(
            "route:other-sink",
            "viewer",
            "viewer:display:1",
            now + Duration::from_millis(5),
        );
        assert!(
            guards
                .take_early_teardown(
                    "route:other-sink",
                    "viewer",
                    now + Duration::from_millis(10),
                )
                .is_none(),
            "an unrelated video sink is not a monitor-switch successor"
        );

        guards.note_start(
            "route:late",
            "viewer",
            "viewer:display:0",
            now + Duration::from_millis(5),
        );
        assert!(
            guards
                .take_early_teardown(
                    "route:late",
                    "viewer",
                    now + Duration::from_millis(5)
                        + VIDEO_SWITCH_TEARDOWN_GUARD
                        + Duration::from_millis(1),
                )
                .is_none(),
            "a deliberate close outside the narrow race window always wins"
        );
    }

    #[test]
    fn monitor_switch_quarantine_is_canceled_or_committed_exactly_once() {
        let now = Instant::now();
        let mut guards = VideoSwitchGuards::default();
        guards.note_stop("route:old-a", "viewer", "viewer:display:0", now);
        guards.note_start(
            "route:new-a",
            "viewer",
            "viewer:display:0",
            now + Duration::from_millis(5),
        );
        let InboundVideoTeardownGate::Quarantine {
            token: canceled,
            incarnation: canceled_incarnation,
            ..
        } = guards.gate_inbound_teardown("route:new-a", "viewer", now + Duration::from_millis(7))
        else {
            panic!("the early close should arm a quarantine");
        };
        assert_eq!(guards.cancel_pending("route:new-a"), Some(canceled));
        assert!(!guards.take_pending_if_current("route:new-a", canceled, canceled_incarnation));

        guards.note_stop("route:old-b", "viewer", "viewer:display:0", now);
        guards.note_start(
            "route:new-b",
            "viewer",
            "viewer:display:0",
            now + Duration::from_millis(5),
        );
        let InboundVideoTeardownGate::Quarantine {
            token: expires,
            incarnation: expires_incarnation,
            ..
        } = guards.gate_inbound_teardown("route:new-b", "viewer", now + Duration::from_millis(7))
        else {
            panic!("the second route should independently arm a quarantine");
        };
        assert!(guards.take_pending_if_current("route:new-b", expires, expires_incarnation));
        assert!(
            !guards.take_pending_if_current("route:new-b", expires, expires_incarnation),
            "an expired timer cannot commit the route twice"
        );

        guards.note_stop("route:old-c", "viewer", "viewer:display:0", now);
        guards.note_start(
            "route:new-c",
            "viewer",
            "viewer:display:0",
            now + Duration::from_millis(5),
        );
        let InboundVideoTeardownGate::Quarantine {
            token: first,
            incarnation: first_incarnation,
            ..
        } = guards.gate_inbound_teardown("route:new-c", "viewer", now + Duration::from_millis(7))
        else {
            panic!("the first close should arm a quarantine");
        };
        assert!(matches!(
            guards.gate_inbound_teardown(
                "route:new-c",
                "viewer",
                now + Duration::from_millis(8),
            ),
            InboundVideoTeardownGate::CoalesceDuplicate { token } if token == first
        ));
        assert!(
            guards.take_pending_if_current("route:new-c", first, first_incarnation),
            "duplicate closes share the original bounded timer"
        );
    }

    #[test]
    fn monitor_switch_reoffer_invalidates_an_old_quarantine_token() {
        let now = Instant::now();
        let mut guards = VideoSwitchGuards::default();
        guards.note_stop("route:old", "viewer", "viewer:display:0", now);
        guards.note_start(
            "route:stable-id",
            "viewer",
            "viewer:display:0",
            now + Duration::from_millis(5),
        );
        let InboundVideoTeardownGate::Quarantine {
            token: old_token,
            incarnation: old_incarnation,
            ..
        } = guards.gate_inbound_teardown(
            "route:stable-id",
            "viewer",
            now + Duration::from_millis(7),
        )
        else {
            panic!("the stale close should be initially eligible");
        };

        guards.note_start(
            "route:stable-id",
            "viewer",
            "viewer:display:0",
            now + Duration::from_millis(8),
        );
        assert!(
            !guards.take_pending_if_current("route:stable-id", old_token, old_incarnation),
            "a delayed timer from an older same-id incarnation is fenced"
        );
    }

    #[test]
    fn monitor_switch_ignores_inflight_feedback_then_accepts_a_mature_heartbeat() {
        let now = Instant::now();
        let armed_at = now + Duration::from_millis(7);
        let mut guards = VideoSwitchGuards::default();
        guards.note_stop("route:old", "viewer", "viewer:display:0", now);
        guards.note_start(
            "route:new",
            "viewer",
            "viewer:display:0",
            now + Duration::from_millis(5),
        );
        let InboundVideoTeardownGate::Quarantine { token, .. } =
            guards.gate_inbound_teardown("route:new", "viewer", armed_at)
        else {
            panic!("the early close should arm a quarantine");
        };
        assert_eq!(
            guards.cancel_pending_on_mature_liveness(
                "route:new",
                armed_at + VIDEO_TEARDOWN_LIVENESS_MIN_AGE - Duration::from_millis(1),
            ),
            None,
            "control already in flight beside the close is not proof"
        );
        assert_eq!(
            guards.cancel_pending_on_mature_liveness(
                "route:new",
                armed_at + VIDEO_TEARDOWN_LIVENESS_MIN_AGE,
            ),
            Some(token),
            "the next periodic viewer heartbeat proves the successor is live"
        );
    }

    #[test]
    fn zero_fps_feedback_is_still_a_static_viewer_heartbeat() {
        let feedback = ControlMessage::Route(RouteControl::VideoFeedback {
            route_id: "route:static".into(),
            recv_fps: 0,
            decode_fails: 0,
            queue_depth: 0,
            lost_ts_us: None,
            ext: Value::Null,
        });
        assert_eq!(
            inbound_video_feedback_liveness_route_id(&feedback),
            Some("route:static")
        );
        assert_eq!(
            inbound_video_feedback_liveness_route_id(&ControlMessage::Route(
                RouteControl::Refresh {
                    route_id: "route:static".into(),
                },
            )),
            None,
            "one-shot setup/recovery controls are not liveness proof"
        );
    }

    #[test]
    fn local_switch_guard_requires_a_mature_post_disconnect_poll() {
        let disconnect_started = Instant::now();
        assert!(!watcher_poll_proves_liveness(
            disconnect_started + Duration::from_millis(1),
            disconnect_started,
        ));
        assert!(!watcher_poll_proves_liveness(
            disconnect_started + VIDEO_LOCAL_POLL_PROOF_MIN_AGE - Duration::from_millis(1),
            disconnect_started,
        ));
        assert!(watcher_poll_proves_liveness(
            disconnect_started + VIDEO_LOCAL_POLL_PROOF_MIN_AGE,
            disconnect_started,
        ));
        assert!(VIDEO_LOCAL_POLL_OBSERVE > VIDEO_LOCAL_POLL_PROOF_MIN_AGE);
    }

    #[test]
    fn offered_video_media_is_dropped_without_killing_the_successor() {
        assert_eq!(
            inbound_video_disposition_from_facts(Some(&RouteState::Offered), true, true, true),
            InboundVideoDisposition::Pending
        );
        assert_eq!(
            inbound_video_disposition_from_facts(Some(&RouteState::Active), true, true, true),
            InboundVideoDisposition::Accept
        );
        assert_eq!(
            inbound_video_disposition_from_facts(Some(&RouteState::TornDown), true, true, true),
            InboundVideoDisposition::Reject
        );
        assert_eq!(
            inbound_video_disposition_from_facts(Some(&RouteState::Offered), true, true, false),
            InboundVideoDisposition::Reject,
            "the grace applies only to the authenticated route peer"
        );
    }

    #[test]
    fn first_video_gate_accepts_parameter_set_led_hevc_entry() {
        let hevc_vps = [0, 0, 1, 0x40, 0x01];
        assert!(crate::video_decode::is_decode_entry(&hevc_vps));
        assert!(
            !should_hold_first_video_sample(true, false, &hevc_vps),
            "HEVC entry is carried by VPS bytes because the daemon key bit is H.264-shaped"
        );

        let h264_delta = [0, 0, 1, 0x41, 0x9a];
        assert!(should_hold_first_video_sample(true, false, &h264_delta));
        assert!(!should_hold_first_video_sample(true, true, &h264_delta));
        assert!(!should_hold_first_video_sample(false, false, &h264_delta));
    }

    #[test]
    fn high_refresh_pacing_stays_inside_the_frame_slot() {
        assert_eq!(pace_budget(16, 30), std::time::Duration::from_millis(16));
        assert_eq!(pace_budget(16, 60), std::time::Duration::from_millis(15));
        assert!(pace_budget(16, 120) < std::time::Duration::from_millis(8));
        assert!(pace_budget(16, 144) < std::time::Duration::from_millis(7));
    }

    #[test]
    fn pipe_waits_consume_the_absolute_pacing_budget() {
        let start = Instant::now();
        let deadline = start + std::time::Duration::from_millis(10);
        let ask = std::time::Duration::from_millis(5);
        assert_eq!(pace_gap_until(deadline, start, ask), ask);
        assert_eq!(
            pace_gap_until(deadline, start + std::time::Duration::from_millis(8), ask),
            std::time::Duration::from_millis(2)
        );
        assert!(
            pace_gap_until(deadline, start + std::time::Duration::from_millis(11), ask).is_zero()
        );
    }

    #[test]
    fn dropped_h264_holds_deltas_until_an_accepted_keyframe() {
        assert!(suppress_dependent_after_drop(true, Some(false)));
        assert!(!suppress_dependent_after_drop(true, Some(true)));
        assert!(!suppress_dependent_after_drop(false, Some(false)));
        assert!(!suppress_dependent_after_drop(true, None));
    }

    #[test]
    fn recovery_requires_a_delivered_key_from_the_current_epoch() {
        let recovery = VideoRecovery::new("test:epoch");
        let (arm, drops, first_epoch) = recovery.mark_drop(Some(false));
        assert!(arm, "the first loss arms one IDR");
        assert_eq!(drops, 1);
        assert!(recovery.suppresses(Some(false)));

        // A dependent send that raced with recovery is covered by the same
        // repair: it neither advances the epoch nor creates an IDR storm.
        let (arm, drops, current_epoch) = recovery.mark_drop(Some(false));
        assert!(!arm, "dependent losses do not create an IDR storm");
        assert_eq!(drops, 2);
        assert_eq!(first_epoch, current_epoch);

        // A failed key advances the epoch and always re-arms. The older key's
        // eventual success cannot release deltas; only the newest one can.
        let (arm, _, newest_epoch) = recovery.mark_drop(Some(true));
        assert!(arm);
        assert_ne!(current_epoch, newest_epoch);
        assert!(!recovery.note_key_delivered(current_epoch));
        assert!(recovery.suppresses(Some(false)));
        assert!(recovery.note_key_delivered(newest_epoch));
        assert!(!recovery.suppresses(Some(false)));
        assert_eq!(recovery.drops.load(Ordering::Relaxed), 3);
        assert_eq!(recovery.suppressed.load(Ordering::Relaxed), 0);
    }

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

    /// Regression guard for the silent fleet-wide loss of remote control: a
    /// presence advert with *no* owner (early boot, older build, a foreign
    /// bridge) must never read as "this member left the fleet" — the evict it
    /// used to trigger authors a signed tombstone that roster convergence
    /// mirrors onto every device, and input/clipboard are then refused
    /// everywhere while video (ungated) keeps streaming. Only a positively
    /// different advertised owner is departure.
    #[test]
    fn ownerless_adverts_are_not_fleet_departure() {
        // A member that positively names another owner has left us.
        assert!(fleet_departure(Some("pkB"), Some("pkA")));
        // A member still naming us is ours.
        assert!(!fleet_departure(Some("pkA"), Some("pkA")));
        // No owner in the advert: ambiguous — never an eviction trigger.
        assert!(!fleet_departure(None, Some("pkA")));
        // Even when our own id is unknown (mesh not ready), an ownerless
        // advert stays inert; a named one can only be "not us".
        assert!(!fleet_departure(None, None));
        assert!(fleet_departure(Some("pkB"), None));
    }

    /// The clock-skew estimate must blame *us* only when the majority of
    /// peers agree: a two-way split verdicts the value nearer zero (that
    /// peer's clock is wrong, not ours), and a lone peer's sample carries as
    /// itself (the warning then words itself neutrally).
    #[test]
    fn clock_skew_median_is_conservative() {
        assert_eq!(conservative_median(&[]), None);
        assert_eq!(conservative_median(&[60_000]), Some(60_000));
        // Split 2-peer network: verdict is the sane clock, no self-blame.
        assert_eq!(conservative_median(&[0, 60_000]), Some(0));
        // Both peers agree we're off: verdict says so.
        assert_eq!(conservative_median(&[58_000, 60_000]), Some(58_000));
        assert_eq!(
            conservative_median(&[-60_000, -58_000, -59_000]),
            Some(-59_000)
        );
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

    /// The presence boot id is the re-sync trigger: a peer answers another's
    /// advert with its own state only when the boot id is one it hasn't
    /// recorded. A network reset drops our peer caches, so we *refresh* the
    /// boot id (see [`Mesh::prune_unjoined_peers`]) — otherwise the side that
    /// reset re-advertises the same id and the other side, still holding us as
    /// `known`, never re-feeds the state we just threw away (the "refresh on one
    /// side strands the connection until both refresh" bug). Guard the two
    /// invariants that mechanism rests on: the id is never 0 (0 is reserved for
    /// pre-field peers), and a refresh actually changes it.
    #[test]
    fn network_reset_refreshes_a_nonzero_presence_boot_id() {
        let client = Arc::new(ControlClient::new().expect("resolve control socket path"));
        let mesh = Mesh::new(client, Arc::new(NoopSink));
        let before = mesh.boot_id.load(Ordering::Relaxed);
        assert_ne!(
            before, 0,
            "boot id is never 0 — 0 means a peer without the field"
        );
        // What the prune does after clearing a reset network's peer caches.
        mesh.boot_id.store(fresh_boot_id(), Ordering::Relaxed);
        let after = mesh.boot_id.load(Ordering::Relaxed);
        assert_ne!(after, 0, "a refreshed boot id is still non-zero");
        assert_ne!(before, after, "a network reset must change the boot id");
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
    #[cfg(all(unix, feature = "host"))]
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
    fn ordered_send_candidates_tries_slot_then_primary_then_the_rest() {
        let slot = "cec-help".to_string();
        let primary = "joining".to_string();
        let joined = vec![
            "joining".to_string(),
            "allmystuff-local-claim-v1".to_string(),
            "cec-help".to_string(),
            "fleet-mesh".to_string(),
        ];
        // Slot first (last proven), then primary, then the remaining joined
        // networks — each exactly once. This order is what lets a send to a
        // multi-homed peer (a KVM on fleet + local-claim + help mesh at once)
        // fall through to the mesh that actually carries our frames.
        assert_eq!(
            ordered_send_candidates(Some(&slot), Some(&primary), &joined),
            vec![
                "cec-help".to_string(),
                "joining".to_string(),
                "allmystuff-local-claim-v1".to_string(),
                "fleet-mesh".to_string(),
            ]
        );
        // No slot yet (never heard from the peer): primary leads.
        assert_eq!(
            ordered_send_candidates(None, Some(&primary), &joined),
            vec![
                "joining".to_string(),
                "allmystuff-local-claim-v1".to_string(),
                "cec-help".to_string(),
                "fleet-mesh".to_string(),
            ]
        );
        // Nothing known at all: nothing to try.
        assert_eq!(
            ordered_send_candidates(None, None, &[]),
            Vec::<String>::new()
        );
    }

    #[test]
    fn seed_peer_networks_fills_gaps_for_reachable_peers_only() {
        use serde_json::json;
        let mut map: HashMap<String, String> = HashMap::new();
        // An inbound frame already proved this peer reachable on the fleet mesh —
        // that mapping carries traffic to us and must survive the peer-list seed.
        map.insert("alice".into(), "fleet".into());
        let peers = vec![
            // alice is also listed on the public mesh, but her proven mapping stands.
            json!({ "device_id": "alice-AB12C", "status": "active" }),
            // bob is reachable here and unknown to us → learns this network instead
            // of falling back to the primary (the bug: a secondary-only peer shows
            // online + wireable yet every frame went to the wrong mesh).
            json!({ "device_id": "bob-9Z8Y7", "status": "active" }),
            // shelved keeps its data channel open, so it is reachable too.
            json!({ "device_id": "carol", "status": "shelved" }),
            // not reachable yet → no mapping (addressing it now would mis-route).
            json!({ "device_id": "dave", "status": "handshaking" }),
            json!({ "device_id": "erin", "status": "offline" }),
        ];
        seed_peer_networks(&mut map, &peers, "public");
        // Proven inbound mapping is never clobbered…
        assert_eq!(map.get("alice").map(String::as_str), Some("fleet"));
        // …a gap is filled, keyed by canonical pubkey (suffix stripped)…
        assert_eq!(map.get("bob").map(String::as_str), Some("public"));
        assert_eq!(map.get("carol").map(String::as_str), Some("public"));
        // …and an unreachable peer claims no slot.
        assert_eq!(map.get("dave"), None);
        assert_eq!(map.get("erin"), None);
    }

    #[test]
    fn seed_peer_links_classifies_and_keeps_on_unknown() {
        use crate::video::LinkClass;
        use serde_json::json;
        let mut map: HashMap<String, LinkClass> = HashMap::new();
        // First sighting: host↔host is LAN, anything reflexive/relayed is WAN.
        let peers = vec![
            json!({ "device_id": "alice-AB12C",
                    "selected_pair": { "local": "host", "remote": "host" } }),
            json!({ "device_id": "bob",
                    "selected_pair": { "local": "host", "remote": "server_reflexive" } }),
            json!({ "device_id": "carol",
                    "selected_pair": { "local": "relay", "remote": "host" } }),
            // ICE not settled (null pair) and an old daemon (field absent):
            // both stay unclassified.
            json!({ "device_id": "dave", "selected_pair": null }),
            json!({ "device_id": "erin" }),
        ];
        let changed = seed_peer_links(&mut map, &peers);
        assert_eq!(map.get("alice"), Some(&LinkClass::Lan));
        assert_eq!(map.get("bob"), Some(&LinkClass::Wan));
        assert_eq!(map.get("carol"), Some(&LinkClass::Wan));
        assert_eq!(map.get("dave"), None);
        assert_eq!(map.get("erin"), None);
        assert_eq!(
            changed.len(),
            3,
            "every first classification reports as a change"
        );

        // A transient unknown (the daemon clears the pair on an ICE blip)
        // must KEEP the learned class — never downgrade a stream on a wobble.
        let blip = vec![json!({ "device_id": "alice-AB12C", "selected_pair": null })];
        let changed = seed_peer_links(&mut map, &blip);
        assert!(changed.is_empty());
        assert_eq!(map.get("alice"), Some(&LinkClass::Lan));

        // A real reclassification (ICE-restart handoff LAN→STUN) reports the
        // change exactly once; a steady-state repeat reports nothing.
        let handoff = vec![json!({ "device_id": "alice-AB12C",
                "selected_pair": { "local": "host", "remote": "peer_reflexive" } })];
        let changed = seed_peer_links(&mut map, &handoff);
        assert_eq!(changed, vec![("alice".to_string(), LinkClass::Wan)]);
        assert!(seed_peer_links(&mut map, &handoff).is_empty());
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
    fn loopback_is_detected_across_node_id_forms() {
        // The regression that broke local terminals: the front-end builds the
        // route from the *display* id (`<pubkey>-ab3d9`) while the backend's
        // `me` is the *bare* node id (`<pubkey>`). A raw `==` sees them as
        // different machines and tries to offer the local terminal over the
        // wire, where it never comes back. `same_node` compares canonically.
        let me = "k7pubkeybody";
        let display = format!("{me}-ab3d9"); // what the UI mints ids from
        let from = node_of(&format!("{display}:terminal"));
        let to = node_of(&format!("{display}:term-view:1"));
        // Raw equality misses it (the suffix differs)…
        assert_ne!(from, me);
        // …but the canonical self-check the loopback branches now use holds.
        assert!(same_node(&from, me) && same_node(&to, me));

        // A genuinely remote terminal stays non-loopback under the same check.
        let host = node_of("otherpubkey-99xyz:terminal");
        assert!(!same_node(&host, me));
    }

    #[test]
    fn term_send_loopback_check_is_canonical_across_id_forms() {
        // `term_send` decides "is this a terminal to my own machine?" so input
        // (incl. xterm's ConPTY cursor-position reply) goes to the local PTY
        // instead of being framed to a peer. The realistic mixed-form case the
        // bug hit: the UI builds the *host* endpoint from the node-list display
        // id (`<pubkey>-ab3d9:terminal`) but the *viewer* endpoint from
        // `localId`, which equals the backend's bare `me`. A raw `==` on the
        // source then read the loopback as remote and framed the reply to a
        // non-existent peer — leaving a ConPTY shell blank on Windows, where no
        // output flows until that reply arrives.
        let me = "k7pubkeybody";
        let display = format!("{me}-ab3d9");
        let route = term_route(
            &format!("{display}:terminal"),
            &format!("{me}:term-view:abc-1"), // built from localId == me
            MediaKind::Generic,
        );

        // The viewer-side gate (`to` is this machine) passes either way…
        assert!(same_node(&node_of(route.to.as_str()), me));

        // …but the loopback flag keys on the *source*, where the forms differ:
        // the raw `==` the fix replaces misses it; `same_node` catches it, so
        // input short-circuits to the local PTY.
        assert_ne!(
            node_of(route.from.as_str()),
            me,
            "raw == missed the self-route"
        );
        assert!(
            same_node(&node_of(route.from.as_str()), me),
            "canonical check recognises the loopback source"
        );

        // A genuinely remote terminal (shell elsewhere) stays non-loopback, so
        // its input is still framed to the host over the mesh.
        let remote = term_route(
            "otherpubkey-99xyz:terminal",
            &format!("{me}:term-view:abc-2"),
            MediaKind::Generic,
        );
        assert!(
            same_node(&node_of(remote.to.as_str()), me),
            "we're the viewer"
        );
        assert!(
            !same_node(&node_of(remote.from.as_str()), me),
            "a remote shell is not a loopback source"
        );
    }

    #[test]
    fn video_lanes_pin_distinct_per_peer_and_reuse_when_freed() {
        use std::collections::HashMap;
        let mut pins: HashMap<String, u8> = HashMap::new();
        let r0 = "route:host:screen:0→viewerkey-ab3d9:sink".to_string();
        let r1 = "route:host:screen:1→viewerkey-ab3d9:sink".to_string();
        let cap = 8;

        // First screen to this viewer takes lane 0…
        let l0 = free_lane_for_peer(&pins, "viewerkey", &r0, cap).unwrap();
        pins.insert(r0.clone(), l0);
        // …the second can NOT reuse it — it must get a fresh lane.
        let l1 = free_lane_for_peer(&pins, "viewerkey", &r1, cap).unwrap();
        pins.insert(r1.clone(), l1);
        assert_ne!(l0, l1, "two screens to one viewer never share a lane");
        assert_eq!((l0, l1), (0, 1));

        // Asking again for an already-pinned route returns its pin (idempotent).
        assert_eq!(free_lane_for_peer(&pins, "viewerkey", &r0, cap), Some(0));

        // A route to a DIFFERENT viewer is independent — it can reuse lane 0.
        let other = "route:host:screen:0→otherkey-77zzz:sink".to_string();
        assert_eq!(free_lane_for_peer(&pins, "otherkey", &other, cap), Some(0));

        // Freeing the first screen's pin lets the next route reuse lane 0.
        pins.remove(&r0);
        let r2 = "route:host:screen:2→viewerkey-ab3d9:sink".to_string();
        assert_eq!(free_lane_for_peer(&pins, "viewerkey", &r2, cap), Some(0));

        // A full pool yields None (the extra stream falls back to MJPEG).
        let mut full: HashMap<String, u8> = HashMap::new();
        for l in 0..2u8 {
            full.insert(format!("route:host:screen:{l}→viewerkey-ab3d9:sink"), l);
        }
        let r_extra = "route:host:screen:9→viewerkey-ab3d9:sink".to_string();
        assert_eq!(free_lane_for_peer(&full, "viewerkey", &r_extra, 2), None);
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
    fn mapped_drive_routes_require_storage_and_the_dedicated_sink() {
        let mapped = term_route("host:disk:E:\\", "viewer:storage-in", MediaKind::Storage);
        assert!(is_mapped_drive_route(&mapped));
        assert_eq!(route_drive_plane(&mapped), Some(DrivePlane::Files));

        let wrong_sink = term_route("host:disk:E:\\", "viewer:disk:C:\\", MediaKind::Storage);
        assert!(!is_mapped_drive_route(&wrong_sink));
        let wrong_media = term_route("host:disk:E:\\", "viewer:storage-in", MediaKind::Generic);
        assert!(!is_mapped_drive_route(&wrong_media));
    }

    #[test]
    fn drive_push_start_requires_fleet_or_a_live_dialed_support_session() {
        assert!(drive_push_may_start(true, false, false));
        assert!(drive_push_may_start(false, true, true));
        assert!(!drive_push_may_start(false, true, false));
        assert!(!drive_push_may_start(false, false, true));
        assert!(!drive_push_may_start(false, false, false));
    }

    #[test]
    fn mapped_drive_offer_accepts_a_pull_token_or_the_files_plane() {
        assert!(mapped_drive_offer_authorized(true, false));
        assert!(mapped_drive_offer_authorized(false, true));
        assert!(mapped_drive_offer_authorized(true, true));
        assert!(!mapped_drive_offer_authorized(false, false));
    }

    #[test]
    fn route_endpoint_placement_ignores_display_suffixes() {
        let route = term_route(
            "source-A3285:drive-map:abc",
            "viewer:storage-in",
            MediaKind::Storage,
        );
        assert!(route_sources_on(&route, "source"));
        assert!(route_sources_on(&route, "source-OTHER"));
        assert!(route_sinks_on(&route, "viewer-0A307"));
        assert!(!route_sinks_on(&route, "someone-else"));
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
    fn cec_identity_never_narrows_ordinary_screen_authority() {
        // A known technician without live CEC consent is blocked only when
        // CEC is the sole possible authority.
        assert!(cec_screen_consent_blocks(false, true));
        // Owner/fleet or an explicit AllMyStuff share wins even if this same
        // identity is also remembered as a CEC technician.
        assert!(!cec_screen_consent_blocks(true, true));
        // With live CEC consent there is no CEC denial in either case.
        assert!(!cec_screen_consent_blocks(false, false));
        assert!(!cec_screen_consent_blocks(true, false));
    }

    #[test]
    fn share_grants_authorize_exactly_their_own_plane() {
        use allmystuff_graph::GrantRole;
        let g = |media: MediaKind, role: GrantRole, cap: &str| Grant {
            id: "g".into(),
            media,
            role,
            capability: Some(cap.into()),
            label: String::new(),
        };

        // A control grant injects — and opens neither a shell, the disk, nor
        // anything else.
        let control = g(MediaKind::Input, GrantRole::Consume, "me:control");
        assert!(grant_authorizes_plane(&control, DrivePlane::Input));
        for p in [
            DrivePlane::Terminal,
            DrivePlane::Files,
            DrivePlane::Sites,
            DrivePlane::Clipboard,
        ] {
            assert!(
                !grant_authorizes_plane(&control, p),
                "control leaked to {p:?}"
            );
        }

        // Terminal and Sites are both Generic grants — the capability suffix is
        // what tells them apart, so neither is mistaken for the other.
        let terminal = g(MediaKind::Generic, GrantRole::Provide, "me:terminal");
        assert!(grant_authorizes_plane(&terminal, DrivePlane::Terminal));
        assert!(!grant_authorizes_plane(&terminal, DrivePlane::Sites));
        let sites = g(MediaKind::Generic, GrantRole::Provide, "me:sites");
        assert!(grant_authorizes_plane(&sites, DrivePlane::Sites));
        assert!(!grant_authorizes_plane(&sites, DrivePlane::Terminal));

        // Files is a storage grant; clipboard its own kind.
        let files = g(MediaKind::Storage, GrantRole::Both, "me:files");
        assert!(grant_authorizes_plane(&files, DrivePlane::Files));
        assert!(!grant_authorizes_plane(&files, DrivePlane::Input));
        let clip = g(MediaKind::Clipboard, GrantRole::Both, "me:clipboard");
        assert!(grant_authorizes_plane(&clip, DrivePlane::Clipboard));

        // A screen grant (watch only) authorizes NO privileged plane — sharing
        // a screen never hands over control, a shell, the disk, or the
        // clipboard.
        let screen = g(MediaKind::Display, GrantRole::Provide, "me:screen");
        for p in [
            DrivePlane::Input,
            DrivePlane::Terminal,
            DrivePlane::Files,
            DrivePlane::Sites,
            DrivePlane::Clipboard,
        ] {
            assert!(
                !grant_authorizes_plane(&screen, p),
                "screen leaked to {p:?}"
            );
        }

        // route_drive_plane classifies exactly the privileged routes.
        assert_eq!(
            route_drive_plane(&term_route("me:terminal", "them:tv:1", MediaKind::Generic)),
            Some(DrivePlane::Terminal)
        );
        assert_eq!(
            route_drive_plane(&term_route("me:files", "them:fv:1", MediaKind::Generic)),
            Some(DrivePlane::Files)
        );
        assert_eq!(
            route_drive_plane(&term_route("me:site", "them:sv:1", MediaKind::Generic)),
            Some(DrivePlane::Sites)
        );
        assert_eq!(
            route_drive_plane(&term_route("me:mic", "them:speaker", MediaKind::Audio)),
            None
        );
    }

    #[test]
    fn refresh_backoff_interval_grows_each_minute_then_caps() {
        use std::time::Duration;
        // 5 s through the first minute, doubling each further minute up to a
        // 60 s ceiling.
        assert_eq!(profile_req_interval(Duration::ZERO), Duration::from_secs(5));
        assert_eq!(
            profile_req_interval(Duration::from_secs(59)),
            Duration::from_secs(5)
        );
        assert_eq!(
            profile_req_interval(Duration::from_secs(60)),
            Duration::from_secs(10)
        );
        assert_eq!(
            profile_req_interval(Duration::from_secs(120)),
            Duration::from_secs(20)
        );
        assert_eq!(
            profile_req_interval(Duration::from_secs(180)),
            Duration::from_secs(40)
        );
        assert_eq!(
            profile_req_interval(Duration::from_secs(240)),
            Duration::from_secs(60)
        );
        assert_eq!(
            profile_req_interval(Duration::from_secs(3600)),
            Duration::from_secs(60)
        );
        // The ceiling is first reached at the 4-minute mark.
        assert_eq!(profile_req_cap_reached(), Duration::from_secs(240));
    }

    #[test]
    fn refresh_backoff_spaces_requests_and_resets_when_idle() {
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        let at = |secs: u64| t0 + Duration::from_secs(secs);

        // The first request is always allowed.
        let (allow, st) = profile_req_decide(None, t0);
        assert!(allow);

        // A second within the 5 s floor is refused…
        let (allow, st) = profile_req_decide(Some(st), at(3));
        assert!(!allow);
        // …and allowed once the floor passes.
        let (allow, st) = profile_req_decide(Some(st), at(5));
        assert!(allow);

        // Five minutes into a sustained burst the floor has grown to the 60 s
        // ceiling: a request 295 s after the last is fine, but 30 s later is not.
        let (allow, st) = profile_req_decide(Some(st), at(300));
        assert!(allow);
        let (allow, st) = profile_req_decide(Some(st), at(330));
        assert!(!allow);

        // A five-minute quiet spell resets the envelope back to the fast floor.
        let (allow, st) = profile_req_decide(Some(st), at(300 + 5 * 60));
        assert!(allow);
        let base = 300 + 5 * 60;
        let (allow, st) = profile_req_decide(Some(st), at(base + 3));
        assert!(!allow); // 3 s — back under the 5 s floor
        let (allow, _) = profile_req_decide(Some(st), at(base + 5));
        assert!(allow);
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

    #[test]
    fn native_drive_mapping_intents_survive_restart() {
        let dir = std::env::temp_dir().join(format!(
            "amst-drive-intents-{}-{}",
            std::process::id(),
            super::fresh_boot_id()
        ));
        let path = Some(dir.join("allmystuff-drives.json"));
        let intent = super::DriveReconnect {
            source: "source-key".into(),
            root: "/Volumes/Install Media".into(),
            label: "Windows installer".into(),
            mount: "X:".into(),
        };
        let mappings = std::collections::HashMap::from([
            ("route:old".into(), intent.clone()),
            // Duplicate routes can briefly coexist during a reconnect, but
            // the durable store records the user's mapping only once.
            ("route:new".into(), intent.clone()),
        ]);

        assert!(super::persist_drive_reconnects(&path, &mappings));
        let loaded = super::load_drive_reconnects(&path);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded.values().next(), Some(&intent));

        let _ = std::fs::remove_dir_all(dir);
    }
}
