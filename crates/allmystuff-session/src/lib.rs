//! # allmystuff-session
//!
//! The live half of AllMyStuff: what the static [`allmystuff_graph`] model
//! becomes once peers are actually connected. It tracks **who's here**
//! (presence) and **what's running** (the route handshake), as a pure,
//! deterministic state machine — no sockets, no audio devices — so the
//! whole connection lifecycle is unit-tested here and the Tauri backend
//! just feeds it messages and carries out the [`Effect`]s it returns.
//!
//! Lifecycle of a route:
//!
//! ```text
//!   offerer                         receiver
//!     │  Offer(route)  ───────────────►│   (Incoming)
//!     │                                ├─ accept ─► Active + StartMedia
//!     │◄────────────── Accept(id)      │
//!  Active + StartMedia                 │
//!     │  …audio frames flow…           │
//!     │  Teardown(id) ────────────────►│   StopMedia
//! ```

mod audio;
mod media;

use std::collections::{HashMap, VecDeque};

use allmystuff_graph::{NodeId, Route};
use allmystuff_protocol::{
    AppControl, ControlMessage, DriveRouteOffer, NodeProfile, OwnershipControl, RouteControl,
    ShareControl,
};

pub use allmystuff_protocol::{CHANNEL_CONTROL, CHANNEL_PRESENCE};
pub use audio::AudioFrame;
pub use media::{
    ClipboardContentKind, ClipboardEvent, ClipboardFrame, ClipboardItem, FileEntry, FileEvent,
    FileFrame, FileVolume, InputAction, InputEvent, MediaPayload, SiteEvent, SiteFrame, TermEvent,
    TermFrame, VideoAssembler, VideoFrame, VideoStatusFrame, VideoStatusState,
    CLIPBOARD_CHUNK_BYTES, SITE_CHUNK_BYTES,
};

/// Which side of a route we are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// We sent the offer.
    Outbound,
    /// A peer offered it to us.
    Inbound,
}

/// Negotiation state of a single route.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RouteState {
    /// We offered; awaiting the peer's accept.
    Offered,
    /// The peer offered; awaiting our decision (only seen transiently when
    /// auto-accept is off).
    Incoming,
    /// Live — media may flow.
    Active,
    /// The peer said no.
    Rejected { reason: String },
    /// Either side tore it down.
    TornDown,
}

/// A route plus its live state and which peer it runs with.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LiveRoute {
    pub route: Route,
    pub peer: NodeId,
    pub origin: Origin,
    pub state: RouteState,
    /// Video transports the offerer can consume (display routes; see
    /// [`RouteControl::Offer`]). On an outbound route these are what we
    /// asked for; on an inbound one, what the peer can decode — the
    /// side that streams reads them to pick the transport.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub video: Vec<String>,
    /// Audio transports the offerer can consume (audio routes) — the
    /// same contract as `video`, for the mesh's Opus lane.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audio: Vec<String>,
    /// For a **terminal** route, the host-side shell session this route is
    /// (or wants to be) bound to — tmux-style multi-attach. On an *outbound*
    /// (viewer) route it starts as the session the viewer asked to attach to
    /// (`None` = "give me a new shell") and is overwritten with the
    /// **resolved** id the host echoes on `Accept`; on an *inbound* (host)
    /// route it's the attach target the viewer asked for, replaced with the
    /// minted/attached id once the backend has actually opened the session.
    /// `None` everywhere else. The viewer's UI reads it to show "shared with
    /// N" and to re-attach. Skipped on the wire when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub term_session: Option<String>,
    /// Receiver-side native mount request for a mapped Storage route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drive: Option<DriveRouteOffer>,
    /// Transient virtual-room authorization scope, when this route belongs to
    /// a joined call. Kept with the live route so per-frame control checks can
    /// revalidate membership and leaving/expiry can tear it down.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room: Option<String>,
}

impl LiveRoute {
    pub fn is_active(&self) -> bool {
        self.state == RouteState::Active
    }
}

#[derive(Default)]
struct OfferContext {
    session: Option<String>,
    drive: Option<DriveRouteOffer>,
    room: Option<String>,
}

/// Something the backend must do as a result of handling a message. The
/// session never performs I/O itself; it returns intent.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Send this control message to a peer (on [`CHANNEL_CONTROL`]).
    Send {
        peer: NodeId,
        message: ControlMessage,
    },
    /// Start carrying media for this now-active route.
    StartMedia(Route),
    /// Stop carrying media for this route id.
    StopMedia(String),
    /// Force a clean decode entry (an IDR / fresh frame) on a route this
    /// machine streams — the viewer's decoder lost its place and asked.
    RefreshMedia(String),
    /// Restart a streamed route's capture with the viewer's quality
    /// picks (`None` = that dial stays automatic).
    TuneMedia {
        route_id: String,
        max_edge: Option<u32>,
        bitrate: Option<u32>,
        fps: Option<u32>,
        game: bool,
        mode: Option<String>,
        /// Opaque video-backend extension, relayed verbatim — see
        /// [`allmystuff_protocol::RouteControl::Tune`]'s `ext`.
        ext: serde_json::Value,
    },
    /// The viewer of a route this machine streams reported its decode health
    /// (receiver → sender). The backend records it per route to adapt the
    /// stream — recovery cadence now, auto-scaling later.
    VideoFeedback {
        route_id: String,
        recv_fps: u32,
        decode_fails: u32,
        queue_depth: u32,
        lost_ts_us: Option<u64>,
        /// Opaque video-backend extension (bandwidth estimate, delay
        /// trend, future receiver-side signals), relayed verbatim — the
        /// pipeline owns its shape. See
        /// [`allmystuff_protocol::RouteControl::VideoFeedback`]'s `ext`.
        ext: serde_json::Value,
    },
    /// A share negotiation message arrived; apply it against the catalog.
    Share { from: NodeId, message: ShareControl },
    /// An ownership/claim message arrived; the backend applies it against
    /// this device's ownership record (claim mode, recorded owner).
    Ownership {
        from: NodeId,
        message: OwnershipControl,
    },
    /// An app-level command arrived (e.g. "upgrade yourself and restart").
    /// The session has no state to change for it — it just forwards intent;
    /// the backend screens the sender (owner/fleet) and carries it out.
    App { from: NodeId, message: AppControl },
}

/// The live session for this node.
#[derive(Debug)]
pub struct Session {
    me: NodeId,
    peers: HashMap<NodeId, NodeProfile>,
    routes: HashMap<String, LiveRoute>,
    /// Route insertion order, used to bound completed negotiation history.
    /// Active/pending routes are never reaped; rejected/torn-down records only
    /// exist long enough for a UI snapshot to explain how a route ended.
    route_order: VecDeque<String>,
    /// Auto-accept inbound offers from known peers (the offerer already
    /// authorized them against their catalog). The backend can turn this
    /// off to prompt the user per offer.
    pub auto_accept: bool,
}

impl Session {
    pub fn new(me: impl Into<NodeId>) -> Self {
        Session {
            me: me.into(),
            peers: HashMap::new(),
            routes: HashMap::new(),
            route_order: VecDeque::new(),
            auto_accept: true,
        }
    }

    pub fn me(&self) -> &NodeId {
        &self.me
    }

    // ---- presence ----------------------------------------------------

    /// Record (or refresh) a peer's presence advert. Ignores our own echo.
    /// Returns `true` if this was new or changed.
    pub fn apply_presence(&mut self, profile: NodeProfile) -> bool {
        if profile.node == self.me {
            return false;
        }
        match self.peers.get(&profile.node) {
            Some(existing) if existing == &profile => false,
            _ => {
                self.peers.insert(profile.node.clone(), profile);
                true
            }
        }
    }

    pub fn peers(&self) -> impl Iterator<Item = &NodeProfile> {
        self.peers.values()
    }

    pub fn peer(&self, id: &NodeId) -> Option<&NodeProfile> {
        self.peers.get(id)
    }

    /// Drop a peer that left, tearing down any routes with it.
    pub fn drop_peer(&mut self, id: &NodeId) -> Vec<Effect> {
        let canonical = canonical_node(id.as_str());
        self.peers
            .retain(|node, _| canonical_node(node.as_str()) != canonical);
        self.reap_peer_routes(id)
    }

    /// Tear down a peer's active routes WITHOUT forgetting the peer — for
    /// a peer that restarted: its fresh incarnation is alive and welcome,
    /// but every route wired to its previous one is dead on its side, and
    /// ours would keep capturing and encoding into the void indefinitely
    /// (the orphaned-streamer failure: media warned as "no route maps to
    /// it" on the far end for as long as the capture lives).
    pub fn reap_peer_routes(&mut self, id: &NodeId) -> Vec<Effect> {
        let mut effects = Vec::new();
        let ids: Vec<String> = self
            .routes
            .iter()
            .filter(|(_, r)| {
                canonical_node(r.peer.as_str()) == canonical_node(id.as_str()) && r.is_active()
            })
            .map(|(rid, _)| rid.clone())
            .collect();
        for rid in ids {
            if let Some(r) = self.routes.get_mut(&rid) {
                r.state = RouteState::TornDown;
            }
            effects.push(Effect::StopMedia(rid));
        }
        effects
    }

    // ---- routes ------------------------------------------------------

    fn insert_route(&mut self, live: LiveRoute) {
        let id = live.route.id.clone();
        if !self.routes.contains_key(&id) {
            self.route_order.push_back(id.clone());
        }
        self.routes.insert(id, live);
    }

    /// Offer a route to the peer that owns its other endpoint. Records it
    /// as `Outbound`/`Offered` and returns the message to send. `video`
    /// names the transports this side can consume for a display route,
    /// `audio` for an audio route (see [`RouteControl::Offer`]); empty
    /// means the channel fallback (MJPEG / PCM frames).
    pub fn offer(
        &mut self,
        route: Route,
        peer: impl Into<NodeId>,
        video: Vec<String>,
        audio: Vec<String>,
    ) -> ControlMessage {
        self.offer_terminal(route, peer, video, audio, None)
    }

    /// [`offer`](Self::offer) with a terminal **session** to attach to —
    /// the multi-attach entry point. `session = Some(id)` joins that
    /// already-running shell on the host (shared scrollback + keyboard);
    /// `None` mints a new shell (and is exactly what [`offer`](Self::offer)
    /// does). Meaningless on non-terminal routes (the host ignores it).
    pub fn offer_terminal(
        &mut self,
        route: Route,
        peer: impl Into<NodeId>,
        video: Vec<String>,
        audio: Vec<String>,
        session: Option<String>,
    ) -> ControlMessage {
        self.offer_with_drive(route, peer, video, audio, session, None)
    }

    /// Offer a route scoped to a currently joined virtual room. The room id is
    /// authorization context, not a standing grant; both nodes independently
    /// verify their short-lived local membership before using it.
    pub fn offer_room(
        &mut self,
        route: Route,
        peer: impl Into<NodeId>,
        video: Vec<String>,
        audio: Vec<String>,
        room: Option<String>,
    ) -> ControlMessage {
        self.offer_scoped(
            route,
            peer,
            video,
            audio,
            OfferContext {
                room,
                ..OfferContext::default()
            },
        )
    }

    /// Offer a route carrying an optional native-drive mount request.
    pub fn offer_with_drive(
        &mut self,
        route: Route,
        peer: impl Into<NodeId>,
        video: Vec<String>,
        audio: Vec<String>,
        session: Option<String>,
        drive: Option<DriveRouteOffer>,
    ) -> ControlMessage {
        self.offer_scoped(
            route,
            peer,
            video,
            audio,
            OfferContext {
                session,
                drive,
                room: None,
            },
        )
    }

    fn offer_scoped(
        &mut self,
        route: Route,
        peer: impl Into<NodeId>,
        video: Vec<String>,
        audio: Vec<String>,
        context: OfferContext,
    ) -> ControlMessage {
        let OfferContext {
            session,
            drive,
            room,
        } = context;
        let peer = peer.into();
        self.insert_route(LiveRoute {
            route: route.clone(),
            peer,
            origin: Origin::Outbound,
            state: RouteState::Offered,
            video: video.clone(),
            audio: audio.clone(),
            term_session: session.clone(),
            drive: drive.clone(),
            room: room.clone(),
        });
        ControlMessage::Route(RouteControl::Offer {
            route,
            video,
            audio,
            session,
            drive,
            room,
        })
    }

    /// Record the **resolved** terminal session id the backend bound a
    /// route to (the minted `term-N` for a new shell, or the existing id for
    /// an attach), so the snapshot surfaces it to the UI and a later
    /// re-attach knows the id. Host-side, called once `terminal.open` has
    /// run; the matching id then rides the viewer's `Accept`.
    pub fn set_term_session(&mut self, route_id: &str, session: String) {
        if let Some(r) = self.routes.get_mut(route_id) {
            r.term_session = Some(session);
        }
    }

    /// Expire an outbound offer nobody answered: transition it to
    /// `Rejected` with a reason the UI can show, so "awaiting accept" is a
    /// state with a deadline instead of a black screen forever (the far
    /// side's app may not be running even though its daemon — and so its
    /// presence — is). Returns whether anything changed; the caller times
    /// the offers (this state machine is deliberately clock-free) and
    /// refreshes its snapshot on `true`. No message to the peer: there's
    /// nobody answering, and a late `Accept` still lands (the route just
    /// reads rejected here; re-offering mints a fresh route id anyway).
    pub fn expire_offer(&mut self, route_id: &str, reason: impl Into<String>) -> bool {
        match self.routes.get_mut(route_id) {
            Some(r) if r.origin == Origin::Outbound && r.state == RouteState::Offered => {
                r.state = RouteState::Rejected {
                    reason: reason.into(),
                };
                true
            }
            _ => false,
        }
    }

    /// Locally tear a route down. Returns the message to send the peer (if
    /// the route was known) so they stop too.
    pub fn teardown(&mut self, route_id: &str) -> Option<ControlMessage> {
        let r = self.routes.get_mut(route_id)?;
        let peer = r.peer.clone();
        r.state = RouteState::TornDown;
        let _ = peer;
        Some(ControlMessage::Route(RouteControl::Teardown {
            route_id: route_id.to_string(),
        }))
    }

    pub fn routes(&self) -> impl Iterator<Item = &LiveRoute> {
        self.routes.values()
    }

    /// Keep at most `max` completed route records, oldest first. A route's
    /// terminal state is useful briefly (the UI can say rejected/torn down),
    /// but retaining every unique Terminal/Files attempt for the lifetime of
    /// the service turns a transient reconnect loop into an unbounded map.
    /// Negotiating and active routes are never removed.
    pub fn prune_completed_routes(&mut self, max: usize) -> usize {
        let completed =
            |r: &LiveRoute| matches!(r.state, RouteState::Rejected { .. } | RouteState::TornDown);
        let mut remove = self
            .routes
            .values()
            .filter(|r| completed(r))
            .count()
            .saturating_sub(max);
        if remove == 0 {
            return 0;
        }
        let mut kept = VecDeque::with_capacity(self.route_order.len());
        let mut removed = 0;
        while let Some(id) = self.route_order.pop_front() {
            match self.routes.get(&id) {
                None => {}
                Some(r) if remove > 0 && completed(r) => {
                    self.routes.remove(&id);
                    remove -= 1;
                    removed += 1;
                }
                Some(_) => kept.push_back(id),
            }
        }
        self.route_order = kept;
        removed
    }

    pub fn route(&self, id: &str) -> Option<&LiveRoute> {
        self.routes.get(id)
    }

    pub fn active_routes(&self) -> impl Iterator<Item = &LiveRoute> {
        self.routes.values().filter(|r| r.is_active())
    }

    // ---- inbound control ---------------------------------------------

    /// Drive the state machine from a control message a peer sent us.
    pub fn handle(&mut self, from: NodeId, message: ControlMessage) -> Vec<Effect> {
        match message {
            ControlMessage::Route(rc) => self.handle_route(from, rc),
            ControlMessage::Share(sc) => vec![Effect::Share { from, message: sc }],
            ControlMessage::Ownership(oc) => vec![Effect::Ownership { from, message: oc }],
            // Site management (list / set-exposed) is handled by the backend
            // before the session ever sees it — it touches no route state —
            // so the state machine just ignores it.
            ControlMessage::Site(_) => Vec::new(),
            // A refresh's "re-announce your presence" ask is answered by the
            // backend directly (it replies with an advert); no route state.
            ControlMessage::ProfileRequest => Vec::new(),
            ControlMessage::App(ac) => vec![Effect::App { from, message: ac }],
            // KVM attach/detach is curated on the **KVM appliance** itself (its
            // Go mesh bridge is the receiver, gated owner/fleet, persisting the
            // binding and re-advertising presence). An ordinary app node only
            // ever *sends* it, so there's no route state to drive here.
            ControlMessage::Kvm(_) => Vec::new(),
            // A control kind a newer peer introduced that this build doesn't
            // know (decoded as `Unknown` rather than failing the message):
            // nothing to drive here.
            ControlMessage::Unknown => Vec::new(),
        }
    }

    fn handle_route(&mut self, from: NodeId, rc: RouteControl) -> Vec<Effect> {
        match rc {
            RouteControl::Offer {
                route,
                video,
                audio,
                session,
                drive,
                room,
            } => {
                // A duplicate Offer for a route we have already accepted and
                // started. The daemon redelivers the same Offer once per
                // shared network (and a re-offer can arrive while the route is
                // still live), so without this guard each duplicate re-emits
                // `StartMedia` and re-spawns the host's capture pump for a
                // route that is already streaming — two capture backends bound
                // to one monitor, and (with the release profile's
                // `panic = "abort"`) a panic on either aborts the whole host.
                // Re-ack so a genuinely lost `Accept` still lands, but never
                // re-insert (it would clobber a host-resolved `term_session`)
                // and never restart media.
                if self
                    .routes
                    .get(&route.id)
                    .is_some_and(|r| r.origin == Origin::Inbound && r.is_active())
                {
                    return vec![Effect::Send {
                        peer: from,
                        message: ControlMessage::Route(RouteControl::Accept {
                            route_id: route.id,
                            session: None,
                        }),
                    }];
                }
                let accept = self.auto_accept;
                let state = if accept {
                    RouteState::Active
                } else {
                    RouteState::Incoming
                };
                self.insert_route(LiveRoute {
                    route: route.clone(),
                    peer: from.clone(),
                    origin: Origin::Inbound,
                    state,
                    video,
                    audio,
                    // The session the viewer asked to attach to (a
                    // terminal route); the backend resolves it to the
                    // real id once it opens the shell, via
                    // [`set_term_session`](Self::set_term_session).
                    term_session: session,
                    drive,
                    room,
                });
                if accept {
                    // The resolved terminal session id isn't known until the
                    // backend has actually opened the PTY (it happens while
                    // carrying out `StartMedia`). The host therefore re-sends
                    // the Accept carrying the resolved id from `start_media`;
                    // this first Accept (no session) starts the viewer's
                    // media at once, exactly as v1.
                    vec![
                        Effect::Send {
                            peer: from,
                            message: ControlMessage::Route(RouteControl::Accept {
                                route_id: route.id.clone(),
                                session: None,
                            }),
                        },
                        Effect::StartMedia(route),
                    ]
                } else {
                    Vec::new()
                }
            }
            RouteControl::Accept { route_id, session } => {
                if let Some(r) = self.routes.get_mut(&route_id) {
                    // The host's accept may echo the resolved terminal session
                    // id (which shell this route actually attached to) — record
                    // it so the viewer's UI can show "shared with N" and
                    // re-attach later. A late accept that only carries the id
                    // (the host's follow-up once the PTY is open) updates the
                    // already-active route without re-starting media.
                    if let Some(s) = session {
                        r.term_session = Some(s);
                    }
                    if r.origin == Origin::Outbound && r.state == RouteState::Offered {
                        r.state = RouteState::Active;
                        return vec![Effect::StartMedia(r.route.clone())];
                    }
                }
                Vec::new()
            }
            RouteControl::Reject { route_id, reason } => {
                // Only the route's own peer may reject it — the same rule
                // Refresh/Tune enforce; without it any node could kill
                // others' routes by name. A reject can now also land on an
                // *active* route (the far side refusing frames it no longer
                // wants, or a receiver NACKing a route it doesn't hold), so
                // an active route stops its media too — otherwise the
                // capture keeps encoding into the void. Origin-blind, like
                // Teardown's stop: a **console host** holds the routes it
                // streams as *inbound* (the viewer offered them), and the
                // old `origin == Outbound` gate silently exempted exactly
                // that side — the receiver's NACK marked the route rejected
                // while the orphan capture streamed on regardless.
                if let Some(r) = self.routes.get_mut(&route_id) {
                    if r.peer == from {
                        let stop = r.is_active();
                        r.state = RouteState::Rejected { reason };
                        if stop {
                            return vec![Effect::StopMedia(route_id)];
                        }
                    }
                }
                Vec::new()
            }
            RouteControl::Refresh { route_id } => {
                // Only honoured for a live route, asked by its own peer —
                // anyone else has no business re-keying the stream.
                if self
                    .routes
                    .get(&route_id)
                    .is_some_and(|r| r.is_active() && r.peer == from)
                {
                    return vec![Effect::RefreshMedia(route_id)];
                }
                Vec::new()
            }
            RouteControl::Tune {
                route_id,
                max_edge,
                bitrate,
                fps,
                game,
                mode,
                ext,
            } => {
                if self
                    .routes
                    .get(&route_id)
                    .is_some_and(|r| r.is_active() && r.peer == from)
                {
                    return vec![Effect::TuneMedia {
                        route_id,
                        max_edge,
                        bitrate,
                        fps,
                        game,
                        mode,
                        ext,
                    }];
                }
                Vec::new()
            }
            RouteControl::VideoFeedback {
                route_id,
                recv_fps,
                decode_fails,
                queue_depth,
                lost_ts_us,
                ext,
            } => {
                // Only the route's own viewer reports on it, and only while
                // it's live — same gate as a refresh/tune ask.
                if self
                    .routes
                    .get(&route_id)
                    .is_some_and(|r| r.is_active() && r.peer == from)
                {
                    return vec![Effect::VideoFeedback {
                        route_id,
                        recv_fps,
                        decode_fails,
                        queue_depth,
                        lost_ts_us,
                        ext,
                    }];
                }
                Vec::new()
            }
            RouteControl::Teardown { route_id } => {
                let mut effects = Vec::new();
                if let Some(r) = self.routes.get_mut(&route_id) {
                    let was_active = r.is_active();
                    r.state = RouteState::TornDown;
                    if was_active {
                        effects.push(Effect::StopMedia(route_id));
                    }
                }
                effects
            }
            // The terminal-sessions picker plane (a viewer asking a host to
            // list its open shells, and the host's answer) touches no route
            // state — the backend handles it directly against the terminal
            // host, gated owner/fleet, exactly like site management. The
            // state machine just ignores it.
            RouteControl::TerminalSessionsRequest | RouteControl::TerminalSessions { .. } => {
                Vec::new()
            }
            // The streamer's lane↔route binding is consumed by the backend's
            // media plane (it routes inbound H.264), not the state machine —
            // handled in the mesh before it reaches here, like the terminal
            // picker plane above. The session just ignores it.
            RouteControl::VideoLane { .. } => Vec::new(),
            // The lane-shaped NACK is likewise the mesh's to translate: only
            // the backend knows its lane→route pins, so it resolves the lane
            // and feeds the result back through this state machine as a
            // plain [`RouteControl::Reject`] — which is where the peer check
            // and the StopMedia decision actually happen. Untranslated, the
            // lane number means nothing to route state.
            RouteControl::DeadLane { .. } => Vec::new(),
            // A route-control kind a newer peer introduced that this build
            // doesn't know (decoded as `Unknown` rather than failing the
            // whole control message): no state change.
            RouteControl::Unknown => Vec::new(),
        }
    }
}

/// Session profiles carry display ids (`pubkey-ABCDE`) while a route peer is
/// often the daemon's bare pubkey. Treat both spellings as the same node when
/// reaping a restarted peer; exact string equality left mapped drives and
/// streams attached to the dead incarnation.
fn canonical_node(device_id: &str) -> &str {
    if let Some((head, tail)) = device_id.rsplit_once('-') {
        if tail.len() == 5 && tail.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            return head;
        }
    }
    device_id
}

#[cfg(test)]
mod tests {
    use super::*;
    use allmystuff_graph::{Flow, MediaKind};
    use allmystuff_protocol::InventorySummary;

    fn profile(node: &str) -> NodeProfile {
        NodeProfile {
            protocol: allmystuff_protocol::PROTOCOL_VERSION,
            node: node.into(),
            label: node.into(),
            hostname: node.into(),
            summary: InventorySummary {
                os: "linux".into(),
                cpu: "cpu".into(),
                ram_bytes: 1 << 30,
                device_count: 3,
                product: "Test Model".into(),
            },
            capabilities: vec![allmystuff_graph::Capability::new(
                node,
                format!("{node}:mic"),
                "Mic",
                MediaKind::Audio,
                Flow::Source,
                "microphone",
            )],
            owner: None,
            claimable: false,
            boot: 0,
            features: vec![],
            sites: vec![],
            version: String::new(),
            fleet_name: String::new(),
            fleet_owner: String::new(),
            kvm: None,
            sent_at: 0,
        }
    }

    fn route(id: &str) -> Route {
        Route {
            id: id.into(),
            from: "this:mic".into(),
            to: "desk:system-audio".into(),
            media: MediaKind::Audio,
        }
    }

    #[test]
    fn presence_tracks_peers_and_ignores_self_echo() {
        let mut s = Session::new("this");
        assert!(s.apply_presence(profile("desk")));
        assert!(!s.apply_presence(profile("desk"))); // unchanged
        assert!(!s.apply_presence(profile("this"))); // our own echo
        assert_eq!(s.peers().count(), 1);
        assert!(s.peer(&"desk".into()).is_some());
    }

    #[test]
    fn outbound_offer_goes_active_on_accept_and_starts_media() {
        let mut s = Session::new("this");
        let msg = s.offer(route("r1"), "desk", Vec::new(), Vec::new());
        assert!(matches!(
            msg,
            ControlMessage::Route(RouteControl::Offer { .. })
        ));
        assert_eq!(s.route("r1").unwrap().state, RouteState::Offered);

        let effects = s.handle(
            "desk".into(),
            ControlMessage::Route(RouteControl::Accept {
                route_id: "r1".into(),
                session: None,
            }),
        );
        assert_eq!(s.route("r1").unwrap().state, RouteState::Active);
        assert!(matches!(effects.as_slice(), [Effect::StartMedia(r)] if r.id == "r1"));
    }

    #[test]
    fn room_scope_round_trips_into_both_live_route_sides() {
        let mut sender = Session::new("this");
        let msg = sender.offer_room(
            route("room-route"),
            "desk",
            vec!["h264".into()],
            Vec::new(),
            Some("room:this:abc".into()),
        );
        assert_eq!(
            sender.route("room-route").unwrap().room.as_deref(),
            Some("room:this:abc")
        );
        let mut receiver = Session::new("desk");
        receiver.handle("this".into(), msg);
        assert_eq!(
            receiver.route("room-route").unwrap().room.as_deref(),
            Some("room:this:abc")
        );
    }

    #[test]
    fn terminal_attach_session_threads_through_offer_and_accept() {
        // Viewer side: offering with a session to attach records it and puts
        // it on the wire; the host's accept echoing the resolved id updates
        // the live route (so the UI can show "shared with N").
        let mut s = Session::new("this");
        let msg = s.offer_terminal(
            route("t1"),
            "desk",
            Vec::new(),
            Vec::new(),
            Some("term-2".into()),
        );
        assert!(matches!(
            msg,
            ControlMessage::Route(RouteControl::Offer { session: Some(ref id), .. }) if id == "term-2"
        ));
        assert_eq!(
            s.route("t1").unwrap().term_session.as_deref(),
            Some("term-2")
        );

        let effects = s.handle(
            "desk".into(),
            ControlMessage::Route(RouteControl::Accept {
                route_id: "t1".into(),
                session: Some("term-2".into()),
            }),
        );
        assert!(matches!(effects.as_slice(), [Effect::StartMedia(r)] if r.id == "t1"));
        assert_eq!(
            s.route("t1").unwrap().term_session.as_deref(),
            Some("term-2")
        );

        // A bare new-shell offer carries no session; the host's accept may
        // still echo back the *minted* id, which a later accept updates
        // without re-starting media.
        let mut s = Session::new("this");
        s.offer(route("t2"), "desk", Vec::new(), Vec::new());
        assert_eq!(s.route("t2").unwrap().term_session, None);
        s.handle(
            "desk".into(),
            ControlMessage::Route(RouteControl::Accept {
                route_id: "t2".into(),
                session: None,
            }),
        );
        // Host's follow-up accept once the PTY is open, carrying the minted id.
        let fx = s.handle(
            "desk".into(),
            ControlMessage::Route(RouteControl::Accept {
                route_id: "t2".into(),
                session: Some("term-5".into()),
            }),
        );
        assert!(
            fx.is_empty(),
            "a late accept on a live route restarts nothing"
        );
        assert_eq!(
            s.route("t2").unwrap().term_session.as_deref(),
            Some("term-5")
        );
    }

    #[test]
    fn host_records_requested_attach_and_resolves_it() {
        // Host side: an inbound terminal offer carrying a session records the
        // viewer's attach target; the backend then resolves it to the real id.
        let mut s = Session::new("desk");
        s.handle(
            "this".into(),
            ControlMessage::Route(RouteControl::Offer {
                route: route("t1"),
                video: Vec::new(),
                audio: Vec::new(),
                drive: None,
                session: Some("term-2".into()),
                room: None,
            }),
        );
        assert_eq!(
            s.route("t1").unwrap().term_session.as_deref(),
            Some("term-2")
        );
        // The backend opened/attached the PTY and recorded the resolved id.
        s.set_term_session("t1", "term-2".into());
        assert_eq!(
            s.route("t1").unwrap().term_session.as_deref(),
            Some("term-2")
        );
    }

    #[test]
    fn inbound_offer_auto_accepts_and_replies() {
        let mut s = Session::new("desk");
        let effects = s.handle(
            "this".into(),
            ControlMessage::Route(RouteControl::Offer {
                route: route("r1"),
                video: Vec::new(),
                audio: Vec::new(),
                drive: None,
                session: None,
                room: None,
            }),
        );
        assert_eq!(s.route("r1").unwrap().state, RouteState::Active);
        // Replies Accept and starts media.
        assert!(effects.iter().any(|e| matches!(
            e,
            Effect::Send { message: ControlMessage::Route(RouteControl::Accept { route_id, .. }), .. } if route_id == "r1"
        )));
        assert!(effects
            .iter()
            .any(|e| matches!(e, Effect::StartMedia(r) if r.id == "r1")));
    }

    #[test]
    fn duplicate_offer_for_an_active_route_does_not_restart_media() {
        // The daemon redelivers the same Offer once per shared network. The
        // first one accepts + starts media; a second identical Offer for the
        // now-active route must NOT re-emit StartMedia (which would
        // double-start the host's capture pump — two backends on one monitor,
        // fatal under panic=abort). It re-acks (in case the first Accept was
        // lost) and leaves the route untouched.
        let mut s = Session::new("desk");
        s.handle(
            "this".into(),
            ControlMessage::Route(RouteControl::Offer {
                route: route("r1"),
                video: Vec::new(),
                audio: Vec::new(),
                drive: None,
                session: None,
                room: None,
            }),
        );
        // A host-resolved terminal session id is recorded after the first start.
        s.set_term_session("r1", "term-7".into());
        let effects = s.handle(
            "this".into(),
            ControlMessage::Route(RouteControl::Offer {
                route: route("r1"),
                video: Vec::new(),
                audio: Vec::new(),
                drive: None,
                // A re-offer might still carry the viewer's original ask;
                // honouring it must not clobber the resolved id below.
                session: Some("term-1".into()),
                room: None,
            }),
        );
        // No second StartMedia…
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::StartMedia(_))),
            "a duplicate Offer must not restart media"
        );
        // …only a re-ack…
        assert!(effects.iter().any(|e| matches!(
            e,
            Effect::Send { message: ControlMessage::Route(RouteControl::Accept { route_id, .. }), .. } if route_id == "r1"
        )));
        // …and the host-resolved session id survives (not clobbered by the
        // duplicate's re-insert).
        assert_eq!(
            s.route("r1").and_then(|r| r.term_session.as_deref()),
            Some("term-7")
        );
    }

    #[test]
    fn inbound_offer_with_auto_accept_off_waits() {
        let mut s = Session::new("desk");
        s.auto_accept = false;
        let effects = s.handle(
            "this".into(),
            ControlMessage::Route(RouteControl::Offer {
                route: route("r1"),
                video: Vec::new(),
                audio: Vec::new(),
                drive: None,
                session: None,
                room: None,
            }),
        );
        assert_eq!(s.route("r1").unwrap().state, RouteState::Incoming);
        assert!(effects.is_empty());
    }

    #[test]
    fn reject_marks_rejected_with_reason() {
        let mut s = Session::new("this");
        s.offer(route("r1"), "desk", Vec::new(), Vec::new());
        s.handle(
            "desk".into(),
            ControlMessage::Route(RouteControl::Reject {
                route_id: "r1".into(),
                reason: "device busy".into(),
            }),
        );
        assert_eq!(
            s.route("r1").unwrap().state,
            RouteState::Rejected {
                reason: "device busy".into()
            }
        );
    }

    #[test]
    fn teardown_stops_media_on_both_sides() {
        // Receiver: offer accepted → active, then peer tears down.
        let mut s = Session::new("desk");
        s.handle(
            "this".into(),
            ControlMessage::Route(RouteControl::Offer {
                route: route("r1"),
                video: Vec::new(),
                audio: Vec::new(),
                drive: None,
                session: None,
                room: None,
            }),
        );
        let effects = s.handle(
            "this".into(),
            ControlMessage::Route(RouteControl::Teardown {
                route_id: "r1".into(),
            }),
        );
        assert_eq!(s.route("r1").unwrap().state, RouteState::TornDown);
        assert!(matches!(effects.as_slice(), [Effect::StopMedia(id)] if id == "r1"));
    }

    #[test]
    fn dropping_a_peer_tears_down_its_active_routes() {
        let mut s = Session::new("desk");
        s.handle(
            "this".into(),
            ControlMessage::Route(RouteControl::Offer {
                route: route("r1"),
                video: Vec::new(),
                audio: Vec::new(),
                drive: None,
                session: None,
                room: None,
            }),
        );
        let effects = s.drop_peer(&"this".into());
        assert!(matches!(effects.as_slice(), [Effect::StopMedia(id)] if id == "r1"));
        assert!(s.peer(&"this".into()).is_none());
    }

    #[test]
    fn reaping_a_restarted_peers_routes_keeps_the_peer() {
        // The restart case: the peer's fresh incarnation stays welcome
        // (presence on file, ready to re-offer), only its stale routes go.
        let mut s = Session::new("desk");
        s.handle(
            "this".into(),
            ControlMessage::Route(RouteControl::Offer {
                route: route("r1"),
                video: Vec::new(),
                audio: Vec::new(),
                drive: None,
                session: None,
                room: None,
            }),
        );
        let had_peer = s.peer(&"this".into()).is_some();
        let effects = s.reap_peer_routes(&"this".into());
        assert!(matches!(effects.as_slice(), [Effect::StopMedia(id)] if id == "r1"));
        assert_eq!(s.peer(&"this".into()).is_some(), had_peer);
        // A second reap finds nothing active — it never double-stops.
        assert!(s.reap_peer_routes(&"this".into()).is_empty());
    }

    #[test]
    fn restarted_display_id_reaps_routes_recorded_against_the_bare_pubkey() {
        let mut s = Session::new("desk");
        s.handle(
            "peerpubkey".into(),
            ControlMessage::Route(RouteControl::Offer {
                route: route("r1"),
                video: Vec::new(),
                audio: Vec::new(),
                drive: None,
                session: None,
                room: None,
            }),
        );
        let effects = s.reap_peer_routes(&"peerpubkey-A1B2C".into());
        assert!(matches!(effects.as_slice(), [Effect::StopMedia(id)] if id == "r1"));
        assert_eq!(s.route("r1").unwrap().state, RouteState::TornDown);
    }

    #[test]
    fn reject_stops_media_on_an_active_outbound_route_and_only_from_its_peer() {
        let mut s = Session::new("desk");
        let _ = s.offer(route("r1"), "peer", Vec::new(), Vec::new());
        let fx = s.handle(
            "peer".into(),
            ControlMessage::Route(RouteControl::Accept {
                route_id: "r1".into(),
                session: None,
            }),
        );
        assert!(matches!(fx.as_slice(), [Effect::StartMedia(_)]));
        // A stranger's reject is ignored — any node could otherwise kill
        // routes by name.
        let fx = s.handle(
            "mallory".into(),
            ControlMessage::Route(RouteControl::Reject {
                route_id: "r1".into(),
                reason: "nope".into(),
            }),
        );
        assert!(fx.is_empty());
        assert!(s.route("r1").unwrap().is_active());
        // The route's own peer refusing an *active* route stops the media —
        // the receiver NACKing frames it can't place must halt the encoder,
        // not leave it streaming into the void.
        let fx = s.handle(
            "peer".into(),
            ControlMessage::Route(RouteControl::Reject {
                route_id: "r1".into(),
                reason: "route not live here".into(),
            }),
        );
        assert!(matches!(fx.as_slice(), [Effect::StopMedia(id)] if id == "r1"));
        assert!(matches!(
            s.route("r1").unwrap().state,
            RouteState::Rejected { .. }
        ));
    }

    #[test]
    fn reject_stops_media_on_an_active_inbound_route_too() {
        // The console-host topology: the VIEWER offers a display route, so
        // the machine that captures and streams holds it as *inbound*. The
        // viewer NACKing frames it can't place (its app restarted, its side
        // of the route died) must stop that capture — the old
        // origin==Outbound gate exempted exactly this side, so the orphan
        // encoder streamed into the void while the route read "rejected".
        let mut s = Session::new("host");
        let fx = s.handle(
            "viewer".into(),
            ControlMessage::Route(RouteControl::Offer {
                route: route("r1"),
                video: Vec::new(),
                audio: Vec::new(),
                drive: None,
                session: None,
                room: None,
            }),
        );
        assert!(fx.iter().any(|e| matches!(e, Effect::StartMedia(_))));
        // A stranger's reject still bounces off the peer check.
        let fx = s.handle(
            "mallory".into(),
            ControlMessage::Route(RouteControl::Reject {
                route_id: "r1".into(),
                reason: "nope".into(),
            }),
        );
        assert!(fx.is_empty());
        assert!(s.route("r1").unwrap().is_active());
        // The route's own viewer refusing the stream halts the capture.
        let fx = s.handle(
            "viewer".into(),
            ControlMessage::Route(RouteControl::Reject {
                route_id: "r1".into(),
                reason: "route not live here".into(),
            }),
        );
        assert!(matches!(fx.as_slice(), [Effect::StopMedia(id)] if id == "r1"));
        assert!(matches!(
            s.route("r1").unwrap().state,
            RouteState::Rejected { .. }
        ));
    }

    #[test]
    fn unanswered_offers_expire_to_rejected_with_a_reason() {
        let mut s = Session::new("desk");
        let _ = s.offer(route("r1"), "peer", Vec::new(), Vec::new());
        assert!(s.expire_offer("r1", "no answer"));
        match &s.route("r1").unwrap().state {
            RouteState::Rejected { reason } => assert_eq!(reason, "no answer"),
            other => panic!("expected Rejected, got {other:?}"),
        }
        // Idempotent: an already-expired (or unknown) offer is a no-op…
        assert!(!s.expire_offer("r1", "again"));
        assert!(!s.expire_offer("r-unknown", "??"));
        // …and an *answered* route can never expire.
        let _ = s.offer(route("r2"), "peer", Vec::new(), Vec::new());
        let _ = s.handle(
            "peer".into(),
            ControlMessage::Route(RouteControl::Accept {
                route_id: "r2".into(),
                session: None,
            }),
        );
        assert!(!s.expire_offer("r2", "too late"));
        assert!(s.route("r2").unwrap().is_active());
    }

    #[test]
    fn completed_route_history_is_bounded_without_touching_live_routes() {
        let mut s = Session::new("desk");
        for n in 0..5 {
            let id = format!("done-{n}");
            let _ = s.offer(route(&id), "peer", Vec::new(), Vec::new());
            assert!(s.expire_offer(&id, "no answer"));
        }
        let _ = s.offer(route("live"), "peer", Vec::new(), Vec::new());
        let _ = s.handle(
            "peer".into(),
            ControlMessage::Route(RouteControl::Accept {
                route_id: "live".into(),
                session: None,
            }),
        );

        assert_eq!(s.prune_completed_routes(2), 3);
        assert!(s.route("done-0").is_none());
        assert!(s.route("done-1").is_none());
        assert!(s.route("done-2").is_none());
        assert!(matches!(
            s.route("done-3").unwrap().state,
            RouteState::Rejected { .. }
        ));
        assert!(matches!(
            s.route("done-4").unwrap().state,
            RouteState::Rejected { .. }
        ));
        assert!(s.route("live").unwrap().is_active());
    }

    #[test]
    fn refresh_and_tune_act_only_on_live_routes_from_their_peer() {
        let mut s = Session::new("desk");
        s.handle(
            "this".into(),
            ControlMessage::Route(RouteControl::Offer {
                route: route("r1"),
                video: Vec::new(),
                audio: Vec::new(),
                drive: None,
                session: None,
                room: None,
            }),
        );
        // The route's peer may re-key and tune it.
        let fx = s.handle(
            "this".into(),
            ControlMessage::Route(RouteControl::Refresh {
                route_id: "r1".into(),
            }),
        );
        assert!(matches!(fx.as_slice(), [Effect::RefreshMedia(id)] if id == "r1"));
        let fx = s.handle(
            "this".into(),
            ControlMessage::Route(RouteControl::Tune {
                route_id: "r1".into(),
                max_edge: Some(1920),
                bitrate: None,
                fps: Some(60),
                game: false,
                mode: None,
                ext: serde_json::Value::Null,
            }),
        );
        assert!(matches!(
            fx.as_slice(),
            [Effect::TuneMedia { route_id, max_edge: Some(1920), bitrate: None, fps: Some(60), game: false, mode: None, .. }]
                if route_id == "r1"
        ));
        // …and report its decode health back, carrying an opaque pipeline
        // ext the session must relay verbatim without inspecting it.
        let fx = s.handle(
            "this".into(),
            ControlMessage::Route(RouteControl::VideoFeedback {
                route_id: "r1".into(),
                recv_fps: 28,
                decode_fails: 3,
                queue_depth: 1,
                lost_ts_us: Some(123_456),
                ext: serde_json::json!({ "est_kbps": 12_000, "delay_trend_us_per_s": -40 }),
            }),
        );
        let relayed = matches!(
            fx.as_slice(),
            [Effect::VideoFeedback {
                route_id,
                recv_fps: 28,
                decode_fails: 3,
                queue_depth: 1,
                lost_ts_us: Some(123_456),
                ext,
            }]
                if route_id == "r1" && ext["est_kbps"] == 12_000 && ext["delay_trend_us_per_s"] == -40
        );
        assert!(relayed, "the opaque pipeline ext is relayed verbatim");
        // A bystander may not.
        let fx = s.handle(
            "stranger".into(),
            ControlMessage::Route(RouteControl::Refresh {
                route_id: "r1".into(),
            }),
        );
        assert!(fx.is_empty());
        // Nor anyone once the route ended.
        s.handle(
            "this".into(),
            ControlMessage::Route(RouteControl::Teardown {
                route_id: "r1".into(),
            }),
        );
        let fx = s.handle(
            "this".into(),
            ControlMessage::Route(RouteControl::Refresh {
                route_id: "r1".into(),
            }),
        );
        assert!(fx.is_empty());
    }

    #[test]
    fn share_messages_surface_for_the_catalog() {
        let mut s = Session::new("this");
        let effects = s.handle("alex".into(), ControlMessage::Share(ShareControl::Decline));
        assert!(matches!(
            effects.as_slice(),
            [Effect::Share { from, message: ShareControl::Decline }] if from == &NodeId::from("alex")
        ));
    }

    #[test]
    fn ownership_messages_surface_for_the_backend() {
        // A peer claims this device; the session just hands it up as an
        // Ownership effect — the backend decides whether the claim takes.
        let mut s = Session::new("puck");
        let effects = s.handle(
            "phone".into(),
            ControlMessage::Ownership(OwnershipControl::Claim {
                owner: "phone".into(),
            }),
        );
        assert!(matches!(
            effects.as_slice(),
            [Effect::Ownership { from, message: OwnershipControl::Claim { owner } }]
                if from == &NodeId::from("phone") && owner == &NodeId::from("phone")
        ));
    }

    #[test]
    fn app_messages_surface_for_the_backend() {
        // An app-level command (upgrade) is pure intent — the session changes
        // no state, it just forwards it; the backend screens the sender and
        // carries it out.
        let mut s = Session::new("puck");
        let effects = s.handle("laptop".into(), ControlMessage::App(AppControl::Upgrade));
        assert!(matches!(
            effects.as_slice(),
            [Effect::App { from, message: AppControl::Upgrade }] if from == &NodeId::from("laptop")
        ));
    }
}
