//! A hand-kept mirror of the MyOwnMesh daemon's control protocol
//! (`MyOwnMesh/crates/myownmesh/src/control.rs` + `ipc/wire.rs`).
//!
//! Wire format is line-delimited JSON over a local socket — a unix-domain
//! socket at `~/.myownmesh/daemon.sock`, a namespaced pipe on Windows.
//! AllMyStuff talks to `myownmesh serve` exactly the way the MyOwnMesh GUI
//! and MyOwnLLM do, and — like them — mirrors the wire shapes here rather
//! than depending on `myownmesh-core`, so the app builds and tests without
//! the engine workspace present. A drift between this mirror and the
//! daemon surfaces as a JSON parse error on the receiving end, never
//! silent corruption.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

/// Resolve the MyOwnMesh state directory shared by the daemon and every
/// AllMyStuff store. `MYOWNMESH_HOME` names the state directory itself; only
/// the ordinary profile-home fallback gains the `.myownmesh` suffix.
pub fn myownmesh_state_dir() -> Option<PathBuf> {
    myownmesh_state_dir_from(
        std::env::var_os("MYOWNMESH_HOME").map(PathBuf::from),
        dirs::home_dir(),
    )
}

/// Pure form of [`myownmesh_state_dir`], exposed so path semantics can be
/// tested without mutating process-global environment variables.
pub fn myownmesh_state_dir_from(
    configured_state_dir: Option<PathBuf>,
    profile_home: Option<PathBuf>,
) -> Option<PathBuf> {
    configured_state_dir.or_else(|| profile_home.map(|home| home.join(".myownmesh")))
}

/// The daemon's per-connection client handle id. On the wire it's the
/// `Display` string `c<n>` (e.g. `"c42"`), exactly matching
/// `myownmesh::ipc::ClientId` — the daemon parses it back with
/// `FromStr`, so a bare number would be rejected. The daemon hands a
/// client its id in the `EventsSubscribe` ack (`data.client_id`); the
/// client passes it back on every channel/RPC op.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientId(pub u64);

impl std::fmt::Display for ClientId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "c{}", self.0)
    }
}

impl std::str::FromStr for ClientId {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let n = s
            .strip_prefix('c')
            .ok_or_else(|| format!("ClientId must start with 'c', got '{s}'"))?;
        Ok(ClientId(
            n.parse().map_err(|e| format!("ClientId parse: {e}"))?,
        ))
    }
}

impl Serialize for ClientId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ClientId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// Client → daemon request. One JSON object per line, dispatched on `op`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    // ---- status / identity / networks --------------------------------
    Status,
    NetworksList,
    PeersList {
        network: String,
    },
    RosterList {
        network: String,
    },
    RosterApprove {
        network: String,
        device_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    RosterRemove {
        network: String,
        device_id: String,
    },
    IdentityShow,
    IdentitySetLabel {
        label: String,
    },
    NetworkIdGenerate,
    ConfigShow,
    NetworkAdd {
        config: Value,
    },
    NetworkRemove {
        network: String,
        /// Also purge the network's persisted **governance state + roster** — a
        /// genuine *forget* (e.g. leaving a fleet), not just unloading the live
        /// network. Default `false` so disabling/refreshing a network keeps its
        /// signed state for a later rejoin; only a deliberate leave sets it.
        /// Matches the daemon's field; leaving it on disk is what makes a
        /// leave-then-rejoin reload a stale (forked) genesis.
        #[serde(default)]
        purge: bool,
    },
    /// Forget **every** joined network on the daemon at once — a bulk
    /// `NetworkRemove{purge:true}` that tears each down and deletes its roster +
    /// signed governance state, keeping the device identity. The daemon exits
    /// afterward so a fresh one reloads clean (matched by an app restart), rather
    /// than a stale in-memory cache re-persisting what was just removed. Needs a
    /// daemon that speaks this op; an older one replies with a parse error.
    ForgetAllNetworks,
    /// Factory reset — the daemon wipes its **entire** state directory
    /// (`~/.myownmesh`: identity, config, every network, and the co-located
    /// AllMyStuff ownership record) and exits so a fresh daemon mints a new
    /// identity on empty state. The device becomes brand-new to every peer.
    FactoryReset,
    NetworkUpdate {
        config: Value,
    },
    /// Reconnect a joined network in place — redial signaling and renegotiate
    /// ICE without leaving the room (the non-destructive twin of
    /// `NetworkRemove` + `NetworkAdd`). Peers keep their sessions and
    /// app-level state, so this is what the GUI's refresh / reconnect controls
    /// drive. `peer` omitted reconnects every peer on the network; `peer` set
    /// reconnects just that one (a per-node refresh). Mirrors the daemon's
    /// `control::Request::NetworkReconnect`.
    NetworkReconnect {
        network: String,
        #[serde(default)]
        peer: Option<String>,
    },
    /// Deliberately dial **one** peer on a `Silent` network — the CEC Support
    /// "connect to this customer" op. A `Silent` network auto-dials nobody and
    /// never gossips a roster: its peers are merely *visible*
    /// (`PeersList`/sighting) until someone names one to connect. This mirrors
    /// the daemon's `JoinedNetwork::connect_peer(device_id)` — it opens a
    /// signaling offer to `device_id` on `network` and answers inbound offers
    /// as usual, without turning the network chatty. `peer` is the peer's
    /// **bare pubkey** (`public_id`), like `ChannelSendTo`. The field name
    /// `peer` matches the daemon's `network_connect_peer` op on the wire.
    /// Additive: a daemon that predates the op replies with an "unknown op"
    /// error, which the node surfaces as "this myownmesh build has no Silent
    /// dial yet".
    NetworkConnectPeer {
        network: String,
        peer: String,
        /// Record a standing dial (daemon ≥ 0.2.34): the daemon redials
        /// this peer on every announce — even on a Silent network — and
        /// never gives up, with the pin persisted in the network config.
        /// The shape a support session needs to survive the far end
        /// sleeping, roaming, or rebooting. `serde(default)` so the op
        /// stays wire-compatible with older daemons (which ignore it).
        #[serde(default)]
        pin: bool,
        /// When > 0, the daemon holds the reply until the peer reaches
        /// ACTIVE (or the deadline) and reports the real outcome. Pair
        /// with [`ControlClient::request_with_timeout`] — the default
        /// 5 s client read timeout is shorter than any useful wait.
        #[serde(default)]
        wait_ms: u64,
    },

    // ---- event stream ------------------------------------------------
    /// Upgrade this connection to a duplex event socket. After the ack,
    /// every server-initiated line is a [`ServerOut`].
    EventsSubscribe,

    // ---- typed channels (how AllMyStuff syncs with peers) ------------
    ChannelSubscribe {
        client_id: ClientId,
        network: String,
        channel: String,
    },
    ChannelUnsubscribe {
        client_id: ClientId,
        network: String,
        channel: String,
    },
    ChannelSendTo {
        network: String,
        channel: String,
        /// The peer's **bare pubkey** (`public_id`). The daemon's peer set is
        /// keyed by what signaling announces — a display id (`pubkey-SUFFIX`,
        /// the form `IdentityShow` and presence carry) misses it and the
        /// daemon replies "peer not found". Strip the suffix before sending.
        peer: String,
        payload: Value,
    },
    /// Send on a typed channel under the daemon's acknowledged-delivery
    /// contract (daemon ≥ 0.2.34): queued until the peer's link is up,
    /// retransmitted across session rebuilds, and the reply resolves
    /// when the peer's engine has delivered the frame (or errs at TTL /
    /// terminal failure). Replaces application retransmit loops — the
    /// CEC connect handshake rides this. Use
    /// [`ControlClient::request_with_timeout`] sized past `ttl_ms`.
    ChannelSendReliable {
        network: String,
        channel: String,
        /// Bare pubkey, same addressing rule as [`Request::ChannelSendTo`].
        peer: String,
        payload: Value,
        /// Milliseconds before an undelivered frame expires (0 = daemon
        /// default, 60 s).
        #[serde(default)]
        ttl_ms: u64,
    },
    /// Open the lowest free media lane ("video" | "audio") toward a
    /// connected peer (daemon ≥ 0.2.34), returning `{ lane }`. Lanes
    /// also open transparently on first write; this is the explicit
    /// reservation.
    MediaLaneOpen {
        network: String,
        peer: String,
        kind: String,
    },
    /// Close a media lane toward a peer (daemon ≥ 0.2.34; idempotent).
    /// The daemon removes the track and the next renegotiation drops
    /// the m-line — media capacity is paid only while a route runs.
    MediaLaneClose {
        network: String,
        peer: String,
        kind: String,
        lane: u8,
    },
    ChannelSendAll {
        network: String,
        channel: String,
        payload: Value,
    },

    // ---- video track lane (real RTP media, not the data channel) -----
    /// Write one encoded H.264 access unit (Annex-B, base64) onto the
    /// video track lane to `peer` (bare pubkey, like `ChannelSendTo`).
    /// The lane is provisioned on every connection at negotiation;
    /// `duration_us` paces the RTP clock (1/fps).
    VideoSend {
        network: String,
        peer: String,
        /// Which of the peer's video lanes to write to. Defaults to 0 so a
        /// pre-pool daemon (which ignores the field) still drives its single
        /// lane; a lane-pool daemon (myownmesh ≥ 0.2.7) routes it to that track.
        #[serde(default)]
        stream: u8,
        duration_us: u64,
        data: String,
    },
    /// Convert this connection into a dedicated **binary media-track pipe**:
    /// after the daemon's ack, the client streams length-prefixed binary
    /// media frames (H.264 access units and Opus frames) with no base64 and
    /// no per-frame JSON — see the frame format in the node's `media_frame`
    /// codec. Nothing else is sent on this connection. The legacy base64
    /// `VideoSend`/`AudioSend` (and the MJPEG/PCM/route-signaling media
    /// channel) stay on the ordinary JSON pipe, untouched.
    MediaTrackPipe,
    /// Convert this connection into a dedicated **binary media-source pipe**
    /// for the client identified by `client_id` (its `EventsSubscribe` id):
    /// after the ack, the daemon pushes length-prefixed inbound media frames
    /// (`[u32 len][body]`, see [`decode_inbound_frame`]) for everything that
    /// client is subscribed to — H.264/Opus from peers, with no base64 and no
    /// JSON. While this pipe is registered the daemon routes inbound media here
    /// instead of as base64 `video_inbound`/`audio_inbound` on the event
    /// socket. The client sends nothing after the handshake.
    MediaSourcePipe {
        client_id: ClientId,
    },
    /// Route assembled video access units from this network's peers to
    /// this client's event socket as `video_inbound` frames.
    VideoSubscribe {
        client_id: ClientId,
        network: String,
    },
    VideoUnsubscribe {
        client_id: ClientId,
        network: String,
    },

    // ---- audio track lane (real RTP media, not the data channel) -----
    /// Write one encoded Opus frame (base64) onto the audio track lane
    /// to `peer` (bare pubkey, like `ChannelSendTo`). The lane is
    /// provisioned on every connection at negotiation; `duration_us` is
    /// the frame length (20 000 for the canonical Opus frame).
    AudioSend {
        network: String,
        peer: String,
        /// Which of the peer's audio lanes to write to (defaults to 0, exactly
        /// like [`Request::VideoSend`]).
        #[serde(default)]
        stream: u8,
        duration_us: u64,
        data: String,
    },
    /// Route audio frames from this network's peers to this client's
    /// event socket as `audio_inbound` frames.
    AudioSubscribe {
        client_id: ClientId,
        network: String,
    },
    AudioUnsubscribe {
        client_id: ClientId,
        network: String,
    },

    // ---- generic RPC (request/response with one peer) ----------------
    RpcRegister {
        client_id: ClientId,
        network: String,
        method: String,
        streaming: bool,
    },
    RpcUnregister {
        client_id: ClientId,
        network: String,
        method: String,
    },
    RpcRespond {
        request_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ok: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    RpcCall {
        network: String,
        peer: String,
        method: String,
        payload: Value,
    },

    /// Replace this node's advertised capability matrix on a network.
    CapabilitiesSet {
        network: String,
        capabilities: Value,
    },

    // ---- self-update -------------------------------------------------
    UpdateStatus,
    UpdateCheck,
    UpdateApply,
    UpdateSetPrefs {
        prefs: Value,
    },

    // ---- closed-network governance (forwarded to the myownmesh daemon) ----
    //
    // AllMyStuff drives the daemon's closed-network governance + the
    // per-device custody MFA that gates owner/kind changes. These mirror the
    // daemon's `op`/snake_case wire shapes (stringly-typed `to`/`role`, which
    // the daemon's NetworkKind/Role deserialise from snake_case) so the bytes
    // match without depending on `myownmesh-core` here.
    /// Snapshot the signed governance state (kind, roles, transition log,
    /// pending proposals, splits) for a network.
    GovernanceState {
        network: String,
    },
    /// Propose a kind change. `to` is `"open"` or `"closed"`.
    GovernanceProposeKindChange {
        network: String,
        to: String,
        /// Per-device custody second factor, when this device enrolled one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mfa_code: Option<String>,
    },
    /// Propose granting `target` a role: `"member"` | `"controller"` | `"owner"`.
    GovernanceProposeRoleGrant {
        network: String,
        target: String,
        role: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mfa_code: Option<String>,
    },
    /// Propose revoking `target`'s role (back to member).
    GovernanceProposeRoleRevoke {
        network: String,
        target: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mfa_code: Option<String>,
    },
    /// Propose evicting `target` from the closed network's roster entirely
    /// — the propagating removal a fleet uses to kick a lost/stolen device.
    GovernanceProposeEvict {
        network: String,
        target: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mfa_code: Option<String>,
    },
    /// Propose the owner-signed, network-wide topology (daemon ≥ 0.2.36).
    /// `topology` is `"ring"` | `"star"` | `"hubs"` | `"full_mesh"`; `hub`
    /// carries the star hub id or the hubs spec
    /// (`id1,id2[,…][:spoke_redundancy]`). Once ratified, every member's
    /// daemon converges onto the shape — this is how fleet infra hubs are
    /// designated. An older daemon rejects the unknown op; callers surface
    /// that as "update the daemon".
    GovernanceProposeTopology {
        network: String,
        topology: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hub: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mfa_code: Option<String>,
    },
    /// Sign a pending proposal.
    GovernanceSign {
        network: String,
        proposal_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mfa_code: Option<String>,
    },
    /// Deny a pending proposal (single-shot kill switch).
    GovernanceDeny {
        network: String,
        proposal_id: String,
    },
    /// Withdraw a proposal this device floated.
    GovernanceWithdraw {
        network: String,
        proposal_id: String,
    },
    /// Spawn a proposer-initiated split. Returns the derived network id.
    GovernanceSpawnSplit {
        network: String,
        proposal_id: String,
    },
    /// Enroll a per-device TOTP custody lock for `network`. Returns the
    /// secret (base32 + `otpauth://` URI) and one-time recovery codes.
    GovernanceMfaEnroll {
        network: String,
    },
    /// Whether this device holds a custody enrollment for `network`.
    GovernanceMfaStatus {
        network: String,
    },
    /// Remove the custody lock for `network` (requires a valid code).
    GovernanceMfaDisable {
        network: String,
        code: String,
    },
}

/// Daemon → client reply to a one-shot [`Request`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl Response {
    pub fn ok(data: Value) -> Self {
        Response {
            ok: true,
            error: None,
            data: Some(data),
        }
    }
    pub fn err(msg: impl Into<String>) -> Self {
        Response {
            ok: false,
            error: Some(msg.into()),
            data: None,
        }
    }
}

/// Server-initiated frame on a duplex (post-`EventsSubscribe`) socket,
/// dispatched on `kind`. Mirrors `myownmesh::ipc::ServerOut`. Unknown
/// kinds are ignored by design, so the daemon can add variants without
/// breaking us.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServerOut {
    /// A live mesh event (peer state / phase / diagnostics). Carried as
    /// opaque JSON — AllMyStuff forwards it to the front-end as-is, the
    /// same way the MyOwnMesh GUI does.
    Event {
        event: Value,
    },
    /// The subscriber fell behind; `skipped` events were dropped.
    Lagged {
        skipped: u64,
    },
    /// An inbound RPC from a peer for a method this client registered.
    RpcInbound {
        network: String,
        from: String,
        request_id: String,
        method: String,
        payload: Value,
        streaming: bool,
    },
    /// A frame on a typed channel this client subscribed to — the
    /// transport for AllMyStuff's presence / route / share messages.
    ChannelInbound {
        network: String,
        from: String,
        channel: String,
        payload: Value,
    },
    RpcCallStreamChunk {
        request_id: String,
        payload: Value,
    },
    RpcCallStreamEnd {
        request_id: String,
        #[serde(default)]
        error: Option<String>,
    },
    HandlerDisplaced {
        network: String,
        method: String,
        by: String,
    },
}

/// Default daemon control-socket location, recomputed locally so the
/// build stays independent of `myownmesh-core` (matching the GUI's
/// `ControlClient::new`). Honours `MYOWNMESH_HOME`.
#[cfg(unix)]
pub fn default_socket_path() -> Option<std::path::PathBuf> {
    Some(myownmesh_state_dir()?.join("daemon.sock"))
}

#[cfg(test)]
mod state_dir_tests {
    use super::myownmesh_state_dir_from;
    use std::path::PathBuf;

    #[test]
    fn configured_home_is_the_state_directory_itself() {
        let configured = PathBuf::from(r"C:\Users\Chris\.myownmesh");
        assert_eq!(
            myownmesh_state_dir_from(
                Some(configured.clone()),
                Some(PathBuf::from(r"C:\Users\LocalSystem")),
            ),
            Some(configured),
        );
    }

    #[test]
    fn profile_fallback_appends_myownmesh_once() {
        let profile = PathBuf::from(r"C:\Users\Chris");
        assert_eq!(
            myownmesh_state_dir_from(None, Some(profile.clone())),
            Some(profile.join(".myownmesh")),
        );
    }
}

/// On Windows the daemon listens on a namespaced pipe rather than a
/// filesystem path.
#[cfg(not(unix))]
pub fn default_pipe_name() -> &'static str {
    "myownmesh.sock"
}

// ---- binary media-track pipe frame codec -----------------------------------
//
// A [`Request::MediaTrackPipe`] connection carries length-prefixed binary
// frames — one encoded H.264 access unit or Opus frame each — with no base64
// and no per-frame JSON. On the wire each frame is `[u32 len LE][body]`, where
// `body` is what [`encode_media_frame`] / [`decode_media_frame`] produce and
// parse. This codec is mirrored verbatim in the `myownmesh` daemon; keep the
// two byte-for-byte identical (both round-trip tested).

/// `kind` byte for an H.264 access unit.
pub const MEDIA_KIND_VIDEO: u8 = 0;
/// `kind` byte for an Opus frame.
pub const MEDIA_KIND_AUDIO: u8 = 1;
/// Defensive cap on one frame body — a corrupt length never allocates more.
pub const MAX_MEDIA_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// One decoded media-track frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaFrame {
    pub kind: u8,
    pub stream: u8,
    pub duration_us: u64,
    pub network: String,
    pub peer: String,
    pub data: Vec<u8>,
}

/// Serialize a media frame **body** (no length prefix; the pipe writes
/// `(body.len() as u32).to_le_bytes()` ahead of it). Layout:
/// `kind u8 · stream u8 · duration_us u64 · net_len u16 · net · peer_len u16 ·
/// peer · data…`, all integers little-endian.
pub fn encode_media_frame(
    kind: u8,
    stream: u8,
    duration_us: u64,
    network: &str,
    peer: &str,
    data: &[u8],
) -> Vec<u8> {
    let net = network.as_bytes();
    let peer = peer.as_bytes();
    let mut out = Vec::with_capacity(14 + net.len() + peer.len() + data.len());
    out.push(kind);
    out.push(stream);
    out.extend_from_slice(&duration_us.to_le_bytes());
    out.extend_from_slice(&(net.len() as u16).to_le_bytes());
    out.extend_from_slice(net);
    out.extend_from_slice(&(peer.len() as u16).to_le_bytes());
    out.extend_from_slice(peer);
    out.extend_from_slice(data);
    out
}

/// Parse a media frame body (the bytes after the `u32` length prefix).
/// Returns `None` on any truncation or non-UTF-8 id — a malformed frame is
/// dropped, never panics.
pub fn decode_media_frame(body: &[u8]) -> Option<MediaFrame> {
    fn rd<'a>(b: &'a [u8], p: &mut usize, n: usize) -> Option<&'a [u8]> {
        let end = p.checked_add(n)?;
        let s = b.get(*p..end)?;
        *p = end;
        Some(s)
    }
    let mut p = 0;
    let kind = rd(body, &mut p, 1)?[0];
    let stream = rd(body, &mut p, 1)?[0];
    let duration_us = u64::from_le_bytes(rd(body, &mut p, 8)?.try_into().ok()?);
    let net_len = u16::from_le_bytes(rd(body, &mut p, 2)?.try_into().ok()?) as usize;
    let network = std::str::from_utf8(rd(body, &mut p, net_len)?)
        .ok()?
        .to_string();
    let peer_len = u16::from_le_bytes(rd(body, &mut p, 2)?.try_into().ok()?) as usize;
    let peer = std::str::from_utf8(rd(body, &mut p, peer_len)?)
        .ok()?
        .to_string();
    let data = body.get(p..)?.to_vec();
    Some(MediaFrame {
        kind,
        stream,
        duration_us,
        network,
        peer,
        data,
    })
}

// ---- binary media-source pipe (inbound) frame codec ------------------------
//
// The daemon→client direction. A [`Request::MediaSourcePipe`] connection
// carries `[u32 len LE][body]` frames, where `body` is what
// [`encode_inbound_frame`] / [`decode_inbound_frame`] produce and parse. The
// fields mirror the old base64 `video_inbound`/`audio_inbound` events. Mirrored
// verbatim in the daemon; keep byte-for-byte identical (round-trip tested).

/// One decoded inbound media frame (from a peer, arriving at this client).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundFrame {
    pub kind: u8,
    /// Video keyframe flag (always false for audio).
    pub key: bool,
    pub stream: u8,
    pub rtp_timestamp: u32,
    /// Sending peer (display id, as the old `from` field carried).
    pub from: String,
    pub data: Vec<u8>,
}

/// Serialize an inbound frame **body** (no length prefix). Layout:
/// `kind u8 · key u8 · stream u8 · rtp_timestamp u32 · from_len u16 · from ·
/// data…`, integers little-endian.
pub fn encode_inbound_frame(
    kind: u8,
    key: bool,
    stream: u8,
    rtp_timestamp: u32,
    from: &str,
    data: &[u8],
) -> Vec<u8> {
    let from = from.as_bytes();
    let mut out = Vec::with_capacity(9 + from.len() + data.len());
    out.push(kind);
    out.push(key as u8);
    out.push(stream);
    out.extend_from_slice(&rtp_timestamp.to_le_bytes());
    out.extend_from_slice(&(from.len() as u16).to_le_bytes());
    out.extend_from_slice(from);
    out.extend_from_slice(data);
    out
}

/// Parse an inbound frame body. `None` on truncation / non-UTF-8 — dropped,
/// never panics.
pub fn decode_inbound_frame(body: &[u8]) -> Option<InboundFrame> {
    fn rd<'a>(b: &'a [u8], p: &mut usize, n: usize) -> Option<&'a [u8]> {
        let end = p.checked_add(n)?;
        let s = b.get(*p..end)?;
        *p = end;
        Some(s)
    }
    let mut p = 0;
    let kind = rd(body, &mut p, 1)?[0];
    let key = rd(body, &mut p, 1)?[0] != 0;
    let stream = rd(body, &mut p, 1)?[0];
    let rtp_timestamp = u32::from_le_bytes(rd(body, &mut p, 4)?.try_into().ok()?);
    let from_len = u16::from_le_bytes(rd(body, &mut p, 2)?.try_into().ok()?) as usize;
    let from = std::str::from_utf8(rd(body, &mut p, from_len)?)
        .ok()?
        .to_string();
    let data = body.get(p..)?.to_vec();
    Some(InboundFrame {
        kind,
        key,
        stream,
        rtp_timestamp,
        from,
        data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbound_frame_round_trips() {
        let body = encode_inbound_frame(MEDIA_KIND_VIDEO, true, 2, 90_000, "peer-ABC", &[4, 5, 6]);
        let f = decode_inbound_frame(&body).expect("decode");
        assert_eq!(f.kind, MEDIA_KIND_VIDEO);
        assert!(f.key);
        assert_eq!(f.stream, 2);
        assert_eq!(f.rtp_timestamp, 90_000);
        assert_eq!(f.from, "peer-ABC");
        assert_eq!(f.data, vec![4, 5, 6]);
        // Audio frame: no key, empty/short payloads also round-trip.
        let a = encode_inbound_frame(MEDIA_KIND_AUDIO, false, 0, 1, "p", &[]);
        let f = decode_inbound_frame(&a).expect("decode");
        assert!(!f.key);
        assert!(f.data.is_empty());
    }

    #[test]
    fn inbound_frame_truncation_is_none() {
        let body = encode_inbound_frame(MEDIA_KIND_VIDEO, false, 1, 7, "peer", &[1, 2]);
        for cut in 0..9 + "peer".len() {
            assert!(decode_inbound_frame(&body[..cut]).is_none(), "short {cut}");
        }
    }

    #[test]
    fn media_frame_round_trips() {
        let body = encode_media_frame(
            MEDIA_KIND_VIDEO,
            3,
            33_333,
            "home",
            "peerpub",
            &[1, 2, 3, 9],
        );
        let f = decode_media_frame(&body).expect("decode");
        assert_eq!(f.kind, MEDIA_KIND_VIDEO);
        assert_eq!(f.stream, 3);
        assert_eq!(f.duration_us, 33_333);
        assert_eq!(f.network, "home");
        assert_eq!(f.peer, "peerpub");
        assert_eq!(f.data, vec![1, 2, 3, 9]);
    }

    #[test]
    fn media_frame_empty_payload_round_trips() {
        let body = encode_media_frame(MEDIA_KIND_AUDIO, 0, 20_000, "n", "p", &[]);
        let f = decode_media_frame(&body).expect("decode");
        assert_eq!(f.kind, MEDIA_KIND_AUDIO);
        assert!(f.data.is_empty());
    }

    #[test]
    fn media_frame_truncation_is_none_not_panic() {
        let body = encode_media_frame(MEDIA_KIND_VIDEO, 1, 1, "home", "peer", &[7, 7, 7]);
        for cut in 0..body.len() {
            // Any prefix shorter than a full header must decode to None, never panic.
            let got = decode_media_frame(&body[..cut]);
            if cut < 14 + "home".len() + "peer".len() {
                assert!(got.is_none(), "short body {cut} should be None");
            }
        }
    }

    #[test]
    fn request_tags_on_op() {
        let j = serde_json::to_value(Request::Status).unwrap();
        assert_eq!(j["op"], "status");

        let j = serde_json::to_value(Request::PeersList {
            network: "home".into(),
        })
        .unwrap();
        assert_eq!(j["op"], "peers_list");
        assert_eq!(j["network"], "home");
    }

    #[test]
    fn client_id_serialises_as_the_c_string() {
        // The daemon parses client_id with FromStr ("c42"), so it must go
        // on the wire as that string — never a bare number.
        let j = serde_json::to_value(ClientId(42)).unwrap();
        assert_eq!(j, serde_json::json!("c42"));
        let back: ClientId = serde_json::from_value(serde_json::json!("c42")).unwrap();
        assert_eq!(back, ClientId(42));
        assert!(serde_json::from_value::<ClientId>(serde_json::json!(42)).is_err());
    }

    #[test]
    fn channel_send_round_trips() {
        let req = Request::ChannelSendAll {
            network: "home".into(),
            channel: "allmystuff/presence".into(),
            payload: serde_json::json!({"hello": true}),
        };
        let line = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&line).unwrap();
        assert!(matches!(back, Request::ChannelSendAll { .. }));
    }

    #[test]
    fn server_out_dispatches_on_kind() {
        let frame = serde_json::json!({
            "kind": "channel_inbound",
            "network": "home",
            "from": "peerid",
            "channel": "allmystuff/presence",
            "payload": {"x": 1}
        });
        let out: ServerOut = serde_json::from_value(frame).unwrap();
        match out {
            ServerOut::ChannelInbound { channel, .. } => assert_eq!(channel, "allmystuff/presence"),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn unknown_response_fields_are_tolerated() {
        // The daemon's Response may carry extra keys; we only need ok/error/data.
        let r: Response =
            serde_json::from_str(r#"{"ok":true,"data":{"a":1},"extra":"ignored"}"#).unwrap();
        assert!(r.ok);
        assert_eq!(r.data.unwrap()["a"], 1);
    }
}

#[cfg(test)]
mod lane_lifecycle_wire_tests {
    use super::*;

    #[test]
    fn connect_peer_pin_fields_are_wire_additive() {
        // An old client's op (no pin/wait_ms) decodes with the defaults…
        let old = r#"{"op":"network_connect_peer","network":"cec-1","peer":"pk"}"#;
        let req: Request = serde_json::from_str(old).expect("old shape decodes");
        match &req {
            Request::NetworkConnectPeer { pin, wait_ms, .. } => {
                assert!(!pin);
                assert_eq!(*wait_ms, 0);
            }
            other => panic!("wrong variant: {other:?}"),
        }
        // …and the new fields serialize under the same op tag.
        let new = Request::NetworkConnectPeer {
            network: "cec-1".into(),
            peer: "pk".into(),
            pin: true,
            wait_ms: 0,
        };
        let v = serde_json::to_value(&new).unwrap();
        assert_eq!(v["op"], "network_connect_peer");
        assert_eq!(v["pin"], true);
    }

    #[test]
    fn reliable_send_and_lane_ops_round_trip() {
        let send = Request::ChannelSendReliable {
            network: "net".into(),
            channel: "cec.control".into(),
            peer: "pk".into(),
            payload: serde_json::json!({"t": "connect"}),
            ttl_ms: 90_000,
        };
        let v = serde_json::to_value(&send).unwrap();
        assert_eq!(v["op"], "channel_send_reliable");
        let back: Request = serde_json::from_value(v).unwrap();
        assert!(matches!(
            back,
            Request::ChannelSendReliable { ttl_ms: 90_000, .. }
        ));

        let close = Request::MediaLaneClose {
            network: "net".into(),
            peer: "pk".into(),
            kind: "video".into(),
            lane: 3,
        };
        let v = serde_json::to_value(&close).unwrap();
        assert_eq!(v["op"], "media_lane_close");
        assert_eq!(v["lane"], 3);
        let open = Request::MediaLaneOpen {
            network: "net".into(),
            peer: "pk".into(),
            kind: "audio".into(),
        };
        assert_eq!(
            serde_json::to_value(&open).unwrap()["op"],
            "media_lane_open"
        );
    }
}
