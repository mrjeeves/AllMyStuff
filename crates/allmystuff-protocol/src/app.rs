//! AllMyStuff's own peer-to-peer protocol — the messages nodes exchange
//! *over* the mesh (inside the daemon's typed-channel frames) once they're
//! connected. Two channels:
//!
//!  * **presence** — each node broadcasts a [`NodeProfile`]: who it is, a
//!    thumbnail of its hardware, and the capabilities it's willing to wire
//!    up. This is what populates the other side's graph.
//!  * **control** — point-to-point [`ControlMessage`]s that set up and
//!    tear down routes and negotiate shares.
//!
//! Authorization is *not* on the wire: a node only ever advertises or
//! accepts what the local [`allmystuff_graph::Catalog`] already permits.
//! The wire carries intent; the catalog is the gate.

use serde::{Deserialize, Serialize};

use allmystuff_graph::{Capability, Grant, NodeId, Person, Route};

/// Mesh app-id for AllMyStuff peers. Distinct from `myownmesh` and
/// `myownllm` so the ecosystems don't collide on signaling (the same
/// non-interop discipline MyOwnMesh documents).
pub const APP_ID: &str = "allmystuff-cloud-mesh-v1";

/// Bumped when a message shape changes incompatibly. Peers include it in
/// presence so a newer node can downgrade its offers for an older one.
pub const PROTOCOL_VERSION: u32 = 1;

/// Typed-channel name for periodic presence broadcasts.
pub const CHANNEL_PRESENCE: &str = "allmystuff/presence/v1";

/// Typed-channel name for point-to-point route/share control.
pub const CHANNEL_CONTROL: &str = "allmystuff/control/v1";

/// Typed-channel name carrying the media plane (audio frames) of active
/// routes. Frames self-identify by route id, so one channel demuxes them
/// all.
pub const CHANNEL_MEDIA: &str = "allmystuff/media/v1";

/// Typed-channel name for the **owned-fleet** roster gossip. When you adopt
/// a device, the two machines start sharing an [`OwnedRoster`] on this
/// channel — the list of devices one owner has claimed, all linked by a
/// single shared key. It rides the daemon's typed channels exactly like
/// presence, and converges by version the same way a mesh roster does.
pub const CHANNEL_OWNED: &str = "allmystuff/owned/v1";

/// Typed-channel name for **virtual rooms** — the lightweight membership +
/// chat plane of a room (the media itself rides ordinary routes). Only
/// peers that advertise [`FEATURE_ROOMS`] subscribe; an older peer never
/// sees the channel, so the whole plane is additive.
pub const CHANNEL_ROOMS: &str = "allmystuff/rooms/v1";

/// Fleet-private Files canvas metadata. It is subscribed everywhere so a
/// network transition cannot strand it, but receivers accept it only on their
/// authenticated closed fleet network.
pub const CHANNEL_FILES_CANVAS: &str = "allmystuff/files-canvas/v1";

/// Well-known LAN-local claim-rendezvous network. Every AllMyStuff node
/// joins it with daemon signaling `{strategy: "none", mdns: true}` — the
/// network never touches remote infrastructure, so a claimable device
/// and its claimer find each other on the local network only. Claimable
/// presence is advertised here (and on a device's own claim-code
/// network, see [`claim_code_network_id`]) and **nowhere else**; claims
/// arriving via any other network are declined. Claimee devices (a
/// NanoKVM's Go bridge mirrors this constant) join while claimable.
/// FROZEN once shipped — both sides must derive the same room.
pub const LOCAL_CLAIM_NETWORK_ID: &str = "allmystuff-local-claim-v1";

/// Length in bytes of a freshly minted claim code (before base32).
/// Minting itself lives with the ownership store (`node`) — this
/// crate stays dependency-free and only defines the encoding.
pub const CLAIM_CODE_BYTES: usize = 16;

/// Encode claim-code bytes as lowercase RFC 4648 base32, no padding
/// (16 bytes → 26 chars). Hand-rolled so this crate stays
/// dependency-free; FROZEN — NanoKVM's Go bridge produces the same
/// encoding with its stdlib base32.
pub fn claim_code_from_bytes(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut out = String::with_capacity(bytes.len().div_ceil(5) * 8);
    for chunk in bytes.chunks(5) {
        let mut buf = [0u8; 5];
        buf[..chunk.len()].copy_from_slice(chunk);
        let v = u64::from(buf[0]) << 32
            | u64::from(buf[1]) << 24
            | u64::from(buf[2]) << 16
            | u64::from(buf[3]) << 8
            | u64::from(buf[4]);
        // 8 output chars per 5 input bytes; truncate to what the
        // partial chunk actually encodes (ceil(bits/5)).
        let chars = match chunk.len() {
            1 => 2,
            2 => 4,
            3 => 5,
            4 => 7,
            _ => 8,
        };
        for i in 0..chars {
            let shift = 35 - 5 * i;
            out.push(ALPHABET[((v >> shift) & 0x1f) as usize] as char);
        }
    }
    out
}

/// Human display form of a claim code: dash groups of four
/// (`xxxx-xxxx-…`), purely cosmetic.
pub fn format_claim_code(code: &str) -> String {
    code.as_bytes()
        .chunks(4)
        .map(|c| std::str::from_utf8(c).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("-")
}

/// The randomized claim-rendezvous network a remote (WAN) claim rides:
/// `amsclaim-<code>`. The claimee mints the code, joins this network
/// (Nostr + mDNS signaling), and shows the code out-of-band (device web
/// UI, console log); the claimer types it in and joins the same network
/// just long enough to claim. Normalizes display formatting (dashes,
/// case) so both sides derive the identical network id — and therefore
/// the identical signaling room. FROZEN once shipped; NanoKVM's Go
/// bridge mirrors this derivation.
pub fn claim_code_network_id(code: &str) -> String {
    let normalized: String = code
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    format!("amsclaim-{normalized}")
}

/// Capability tag this node advertises on the **mesh** capability matrix
/// (MyOwnMesh `CapabilitiesSet`) — not the bespoke presence channel — to mark
/// itself as a real AllMyStuff app node rather than a bare `myownmesh` daemon.
/// It rides the reliable handshake + peer-list, so a peer learns "this device
/// is on AllMyStuff" from the polled peer view even when a presence advert is
/// dropped. The node's [`NodeProfile::features`] are advertised alongside it as
/// tags, so a peer's action buttons light up from the peer list too.
pub const CAP_TAG_ALLMYSTUFF: &str = "allmystuff";

/// Feature tag a node advertises in [`NodeProfile::features`] when it can
/// host mesh-native terminal sessions (spawn a PTY and stream it over the
/// media channel). A peer only offers a terminal route to nodes that
/// advertise this.
pub const FEATURE_TERMINAL: &str = "terminal";

/// Feature tag a node advertises in [`NodeProfile::features`] when it can
/// host mesh-native file sessions (browse, read and manage its filesystem
/// over the media channel — the "Open Files" console). A peer only offers
/// a files route to nodes that advertise this.
pub const FEATURE_FILES: &str = "files";

/// Feature tag a node advertises in [`NodeProfile::features`] when it
/// speaks the virtual-rooms plane ([`CHANNEL_ROOMS`]): room invites, join /
/// leave presence, and chat. The room UI badges members without it (an
/// older build) so nobody wonders why a message went unanswered.
pub const FEATURE_ROOMS: &str = "rooms";

/// Feature tag a node advertises in [`NodeProfile::features`] when it can
/// host **sites** — reverse-proxy a TCP service it's listening on (a local
/// web app, a database) over the mesh to a peer, who reaches it through a
/// locally-mapped port. The advertised [`NodeProfile::sites`] are the *only*
/// ports such a host will proxy; a peer only offers a site route to a node
/// that advertises this tag, and an older build never sees the plane.
pub const FEATURE_SITES: &str = "sites";

/// Feature tag a node advertises in [`NodeProfile::features`] when it can
/// *stream* its cameras (a capture backend feeds its video routes). The
/// cameras themselves have always ridden presence as capabilities; this
/// says selecting one will actually produce pixels — a console pointed at
/// an older build's camera explains "update that machine" instead of
/// waiting on a stream that will never start.
pub const FEATURE_CAMERA: &str = "camera";

/// Feature tag a node advertises when binary clipboard transfers (images and
/// native file lists) finish with an explicit receiver receipt. A sender that
/// sees this waits for the far OS to stage and publish the content before it
/// follows with a paste keystroke; older peers keep the ordered, best-effort
/// behavior they shipped with.
pub const FEATURE_CLIPBOARD_RECEIPTS: &str = "clipboard-receipts";

/// Feature tag a node advertises in [`NodeProfile::features`] when its local
/// daemon provisions the **media lane pool** (myownmesh ≥ 0.2.7): several
/// independent video/audio RTP tracks per peer, so a sender can fan several
/// simultaneous streams to one peer (two screens of one machine) onto
/// separate lanes instead of one. A sender only routes a second+ stream to a
/// lane past 0 when the receiver advertises this; otherwise the extra stream
/// falls back to MJPEG, exactly as before the pool.
pub const FEATURE_MEDIA_LANES: &str = "media-lanes";

/// Feature tag a node advertises when it speaks the paced-video v1 framing
/// contract. A capable sender may split one encoded access unit across
/// several same-timestamp track samples and closes the train with an explicit
/// marker carrying the expected fragment count. A capable receiver validates
/// that count and reassembles the complete access unit before any decoder sees
/// it. The streaming side selects the mode in [`RouteControl::Accept`]; the
/// feature alone never changes framing. Rolling upgrades therefore keep
/// whole-access-unit delivery until both ends support and acknowledge the
/// contract.
pub const FEATURE_PACED_VIDEO: &str = "paced-video-v1";

/// Feature tag a node advertises in [`NodeProfile::features`] when it is a
/// **KVM appliance** (a NanoKVM-class device): it captures a target machine's
/// HDMI and injects USB-HID into it, and it carries its own web UI as a
/// [`SiteAdvert`]. A peer that sees this tag renders the KVM drawer — the
/// "Open KVM" button (maps + opens [`KvmAdvert::web`]) plus the
/// attach/detach affordance. The KVM's current binding rides presence in
/// [`NodeProfile::kvm`]. Additive: an older peer that doesn't know the tag
/// just sees an ordinary app node.
pub const FEATURE_KVM: &str = "kvm";

/// A thumbnail of a node's hardware — enough for the graph's node card
/// without shipping the whole [`allmystuff_inventory::Inventory`]. The
/// backend fills this from a scan.
///
/// `Default` (all-empty) so a presence advert whose summary is absent, partial,
/// or still being scanned **decodes** rather than dropping the whole profile —
/// the node card just shows the "unknown hardware" fallback until a fuller
/// advert lands. The container `#[serde(default)]` carries that leniency *into*
/// the summary: a partial summary (some hardware fields missing, e.g. an older
/// or mid-scan peer) fills the gaps with defaults instead of failing the whole
/// `NodeProfile` decode. See [`NodeProfile::summary`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct InventorySummary {
    pub os: String,
    pub cpu: String,
    pub ram_bytes: u64,
    /// Headline device count for the "12 things" chip.
    pub device_count: u32,
    /// Product / model name — the machine's own identity ("OptiPlex 7090",
    /// "MacBook Pro"), the DMI product field without its maker prefix. This
    /// rides presence so a CEC technician can tell one customer's box from
    /// another at a glance. Empty when the host reports no usable product
    /// string (a VM, a stripped DMI); `#[serde(default)]` (via the struct's
    /// `#[serde(default)]`) keeps adverts from older peers decoding.
    pub product: String,
}

/// What a node tells its peers about itself. Broadcast on the presence
/// channel when joining and whenever its inventory or offered capabilities
/// change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeProfile {
    /// Presence protocol version, informational only (we never gate inbound
    /// adverts on it). `#[serde(default)]` so an advert omitting it still
    /// decodes rather than being discarded whole.
    #[serde(default)]
    pub protocol: u32,
    pub node: NodeId,
    /// Display name for this node — the machine's hostname by default, or a
    /// user-set override. When it differs from `hostname`, the UI renders
    /// "label (hostname)" so the real machine is always visible.
    /// `#[serde(default)]` so a label-less advert still decodes (the UI falls
    /// back to the hostname / a short id) instead of dropping the whole update.
    #[serde(default)]
    pub label: String,
    /// The node's real machine hostname, always straight from its own scan.
    /// `#[serde(default)]` so presence from an older peer (no hostname field)
    /// still decodes — the UI just falls back to `label`.
    #[serde(default)]
    pub hostname: String,
    /// Hardware thumbnail for the node card. `#[serde(default)]` so an advert
    /// with no summary yet (a peer mid-scan, an older/minimal build, or a
    /// partial summary) still decodes — only this field falls back to empty,
    /// rather than the **entire** profile being discarded in the parse. This
    /// was the central "we get presence updates but silently drop them" bug:
    /// one missing/retyped required field failed the whole `NodeProfile`
    /// decode, so the peer never appeared or never refreshed.
    #[serde(default)]
    pub summary: InventorySummary,
    /// The capabilities this node is willing to expose. The owner curates
    /// this; nothing here is reachable without the receiver's catalog also
    /// authorizing the route.
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    /// Who owns this device, if it has recorded an owner — the node id that
    /// claimed or was configured to own it. A peer reads this to decide
    /// whether the device is *theirs* (owner == them) or someone else's;
    /// a device you don't own can't be silently adopted. `None` = unowned.
    /// `#[serde(default)]` so presence from an older peer still decodes.
    #[serde(default)]
    pub owner: Option<NodeId>,
    /// `true` only when this device was started in **claim mode** and has no
    /// owner yet — i.e. it is *offering* itself to be adopted. Claiming is
    /// refused unless this is set, so you can't flat-out take a device that
    /// hasn't been put up for adoption (it defines its own owner instead).
    #[serde(default)]
    pub claimable: bool,
    /// Random id minted once per app run. Gossip is **event-driven, not a
    /// heartbeat**: a receiver that sees a boot id it hasn't recorded for
    /// this peer knows the peer just (re)started and missed our earlier
    /// adverts, and answers with its own presence + roster directly — so
    /// state converges on the events that change it, with no periodic
    /// re-broadcast bloating the mesh. `0` = an older peer without the
    /// field (those still heartbeat, so no reply is needed).
    #[serde(default)]
    pub boot: u64,
    /// App features this node supports beyond the v1 baseline — e.g.
    /// [`FEATURE_TERMINAL`]. Unknown entries are ignored; absent (an older
    /// peer) decodes as empty, and empty serializes *without* the key so an
    /// older receiver sees exactly the presence shape it always did. A
    /// feature is only ever offered to a peer that advertises it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
    /// The **sites** this node exposes — TCP services it's willing to
    /// reverse-proxy over the mesh (see [`SiteAdvert`]). The owner curates
    /// this; it's the exhaustive set a peer may ask the host to proxy, so a
    /// connection to anything *not* listed is refused (the advert is the
    /// allow-list, not just a hint). Additive: absent (an older peer)
    /// decodes as empty, and empty serializes *without* the key, so the
    /// presence shape an older receiver sees is unchanged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sites: Vec<SiteAdvert>,
    /// The AllMyStuff version this node is running — its binary's
    /// `CARGO_PKG_VERSION` (e.g. `"0.1.11"`). It lets a peer notice that one
    /// of its own machines is behind the channel's latest release and offer
    /// to upgrade it ([`AppControl::Upgrade`]). Absent from an older peer
    /// (`default`) decodes as empty — "unknown" — so the upgrade affordance
    /// simply never appears for it; empty serializes *without* the key, so an
    /// older receiver sees exactly the presence shape it always did.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version: String,
    /// The fleet this device belongs to, by display name — "whatever its owner
    /// answers to" ("Casey"). Carried in presence so a peer can **group** this
    /// device into its fleet and **label** that group without reconstructing
    /// the owner's name from the catalog. Shared fleet-wide: the owner hands it
    /// down with the fleet key on adoption, so every member advertises the same
    /// name. Empty = not in a fleet (or an unnamed one); empty serializes
    /// *without* the key, so an older receiver sees the presence shape it always
    /// did. `#[serde(default)]` so presence from an older peer still decodes.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub fleet_name: String,
    /// The fleet **owner's** display name — the *person* who owns the fleet, not
    /// the owner device's hostname. Lets every peer render "Casey's fleet" the
    /// same way the owner does, sourced from the advert rather than resolved
    /// from whichever device happens to be in the catalog. A fleet is named for
    /// its owner, so today this tracks [`Self::fleet_name`]; the owner device
    /// falls back to its own label only for an as-yet-unnamed fleet. Empty when
    /// unknown; empty serializes *without* the key (additive — older peers see
    /// the unchanged shape).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub fleet_owner: String,
    /// KVM-appliance state, present only on a node that advertises
    /// [`FEATURE_KVM`] (a NanoKVM-class device). Carries what the KVM is
    /// currently *attached to* — the graph node it physically controls — and
    /// which site is its web UI. `None` on every ordinary machine; an older
    /// peer (or a non-KVM) omits it, and `None` serializes *without* the key,
    /// so the presence shape an older receiver sees is unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kvm: Option<KvmAdvert>,
    /// The sender's **wall clock when this advert was stamped for send**
    /// (Unix ms). A receiver reads it as a free clock-skew sample against
    /// its own clock — the basis of the "this device's clock is out of sync
    /// with the network" warning, built entirely from traffic that was
    /// flowing anyway (no extra calls to any node). Stamped per send, never
    /// persisted. `0` = an older sender (no sample); zero serializes
    /// *without* the key, so an older receiver — and the KVM bridge's
    /// pinned Go fixtures — sees exactly the presence shape it always did.
    #[serde(default, skip_serializing_if = "u64_is_zero")]
    pub sent_at: u64,
}

/// `skip_serializing_if` helper for additive numeric presence fields.
fn u64_is_zero(v: &u64) -> bool {
    *v == 0
}

fn is_false(v: &bool) -> bool {
    !*v
}

/// One site a node exposes — a TCP service it's listening on that it's
/// willing to reverse-proxy over the mesh. Rides the presence advert
/// ([`NodeProfile::sites`]); the bytes themselves never touch presence (a
/// connection is a route, tunneled on the media channel). The set is the
/// host's allow-list: it only proxies a port that appears here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteAdvert {
    /// Stable id, mirroring the scan's `ListeningService.id` (`tcp:8080`),
    /// so a mapped site keeps its identity across rescans and restarts.
    pub id: String,
    /// Friendly label — "HTTP", "PostgreSQL", or "Port 8080".
    pub label: String,
    /// The TCP port the host is listening on (and will proxy to on
    /// loopback). The host re-checks an inbound request's port against this
    /// set before connecting, so a peer can't pivot to an unadvertised port.
    pub port: u16,
    /// URL scheme a client reaches it with — "http", "https", "ssh", … — or
    /// empty for a bare TCP service the proxy still tunnels. A web scheme
    /// (`http`/`https`) is what lets the UI offer "open in browser".
    #[serde(default)]
    pub scheme: String,
    /// `true` when the host bound it to loopback only — the prime
    /// reverse-proxy case (not reachable on its LAN, but the mesh carries
    /// it). Cosmetic; the proxy works the same either way.
    #[serde(default)]
    pub loopback: bool,
}

impl SiteAdvert {
    /// `true` for a web service the UI can "open in browser" — its scheme is
    /// `http` or `https`.
    pub fn is_web(&self) -> bool {
        self.scheme == "http" || self.scheme == "https"
    }
}

/// One device in an owned fleet — a machine the same owner has claimed, so
/// it shares the fleet's key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnedMember {
    /// The member's stable device id. Stored in canonical (bare-pubkey) form
    /// so a display-id and bare-pubkey view of one machine collapse to a
    /// single member.
    pub device: NodeId,
    /// Best-known display label for the member (cosmetic; the newest non-empty
    /// label a gossip carries wins).
    #[serde(default)]
    pub label: String,
    /// Unix seconds at which this owner first added the member, or `None` for a
    /// record written before this field existed.
    ///
    /// Load-bearing for exactly one decision: whether a signed eviction still
    /// applies. `ensure_fleet_network` prunes a locally listed device the signed
    /// governance has removed, so one owner's admit loop can't resurrect a
    /// device another owner evicted. Membership converges last-writer-wins on
    /// the entry stamp, so an eviction only outranks a claim that came *before*
    /// it — and without this the prune could not tell the two apart, which made
    /// a deliberate re-claim of a previously evicted device impossible: the
    /// prune ran before the admit loop every time, so the superseding grant the
    /// governance layer would have honoured was never authored.
    ///
    /// `None` is treated as older than any eviction, so a legacy record keeps
    /// the previous (always-prune) behaviour rather than being resurrected by
    /// an upgrade. Re-claiming such a device stamps it and recovers it.
    #[serde(default)]
    pub claimed_at: Option<u64>,
}

/// A snapshot of an owner's fleet for the front-end: the shared key, the
/// fleet name, a change counter, and the members. This is now a **node →
/// GUI** shape only (the `owned_roster` command and the `allmystuff://owned`
/// event) — it is no longer gossiped between peers. The node builds it from
/// local credential state plus the fleet's closed-network **signed roster**,
/// which is the real source of membership truth.
///
/// The key is an **internal grouping secret** — every device in the fleet
/// holds the same one, minted by the first owner to claim a device and handed
/// down on each adoption ([`OwnershipControl::FleetKey`]). Both sides derive
/// the fleet's closed-network id from it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct OwnedRoster {
    /// Shared fleet key. Empty means "no fleet yet".
    #[serde(default)]
    pub key: String,
    /// The fleet's display name — whatever its owner answers to ("Casey").
    /// Cosmetic. Empty = unnamed, and an empty name is skipped on the wire so
    /// an older peer sees exactly the roster shape it always did.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// A local change counter (bumped on claim/kick/rename), surfaced so newer
    /// GUI renders win. No longer a gossip convergence clock.
    #[serde(default)]
    pub version: u64,
    /// The fleet's members, projected from the closed network's signed roster.
    #[serde(default)]
    pub members: Vec<OwnedMember>,
}

/// One file a member offers into a room's **Shared Files** area, as the
/// uploader states it to the host. `token` is an opaque fetch handle the
/// uploader minted (it round-trips back as the files plane's `Fetch`
/// request); the bytes never touch the host — a downloader pulls them
/// straight from the uploader over a `:shared` route, gated on the token
/// and the room's member set. `size` is the file's byte length, for the
/// progress bar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedFileMeta {
    pub token: String,
    pub name: String,
    #[serde(default)]
    pub size: u64,
}

/// One entry of the host's aggregated Shared Files list — a
/// [`SharedFileMeta`] tagged with the uploader so a downloader knows whom
/// to fetch the bytes from. The host hosts the *list*; the uploader hosts
/// the *bytes* (and only while it's online).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedEntry {
    /// The uploader (canonical node id) — whom to open the `:shared` route to.
    pub from: NodeId,
    pub token: String,
    pub name: String,
    #[serde(default)]
    pub size: u64,
}

/// One message of the virtual-rooms plane, carried on [`CHANNEL_ROOMS`].
/// A room itself is a lightweight, user-minted thing — a stable id, a
/// cosmetic name, and a member list — and every message restates the id
/// and name so a receiver can render a room it has never heard of (an
/// invite that raced a chat line, a member that reinstalled). The media
/// of a room (mic, screen share, …) is **not** here: those are ordinary
/// routes, proposed and authorized exactly like any other connection.
///
/// A room is **hosted by its maker**: the id is minted under the maker's
/// canonical device id (`room:{owner}:{nonce}`), and the room's control
/// plane — its roster and name (the [`RoomEvent::Invite`] replacement) and
/// its end of life ([`RoomEvent::Close`]) — is honoured only from that
/// host (the mesh authenticates senders, so this is a real check, not a
/// label). Members talk to each other directly for everything streamed
/// (join/leave presence, chat, the media routes) — nothing flows *through*
/// the host — and a room is stream-only: nothing is stored, and any future
/// history would live with the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomMessage {
    /// The room's stable id (minted by its creator).
    pub room: String,
    /// The room's display name, restated on every message.
    #[serde(default)]
    pub name: String,
    /// What happened.
    #[serde(flatten)]
    pub event: RoomEvent,
}

/// How a room admits a machine that asks to join ([`RoomEvent::Knock`])
/// without holding an invite. The host states it on every
/// [`RoomEvent::Invite`]; only the host ever enforces it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RoomAccess {
    /// Anyone on a shared mesh who knocks with the room's id is admitted
    /// automatically — the id *is* the invite.
    Open,
    /// The host admits each knock by hand. The default, and what an older
    /// host's invites (no field on the wire) read as.
    #[default]
    Invite,
}

// Lenient decode: an access mode a newer host introduced that this build
// doesn't recognise reads as the *safe* default (invite-only) rather than
// failing the whole `Invite` message — so a future access policy can never
// stop an older member from learning about (and being listed in) the room.
// `#[serde(other)]` isn't available on a string-valued enum, so this is the
// hand-rolled equivalent.
impl<'de> Deserialize<'de> for RoomAccess {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(match String::deserialize(d)?.as_str() {
            "open" => RoomAccess::Open,
            _ => RoomAccess::Invite,
        })
    }
}

/// The events of a room's membership + chat plane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RoomEvent {
    /// "You're in this room." Sent by the **host** on creation and on
    /// every roster or name change, carrying the full member list —
    /// replacement semantics, so removals propagate (a member that's no
    /// longer listed drops the room). Receivers ignore invites for a
    /// known room from anyone but its host. Once received, the room stays
    /// on the member's device like a roster slot — it's listed (and can be
    /// rejoined) until the host removes them or closes the room.
    Invite {
        members: Vec<NodeId>,
        /// How the room treats knocks. Absent on an older host's invites
        /// (`default`), which reads as invite-only — never more open.
        #[serde(default)]
        access: RoomAccess,
    },
    /// The sender opened the room — they're present in the call now.
    Join,
    /// The sender left the room (hung up).
    Leave,
    /// A chat line from the sender.
    Chat { text: String },
    /// The **host** closed the room for everyone — members drop it.
    /// (From anyone else it's ignored. An older build doesn't know the
    /// kind and drops the whole message: it just keeps a dead room
    /// listed until the user forgets it locally.)
    Close,
    /// "May I join?" — sent **to the host** by a machine holding the
    /// room's id but no invite (the id was shared out-of-band and pasted
    /// into the rooms UI). An [`RoomAccess::Open`] host admits at once by
    /// re-stating the roster (an [`RoomEvent::Invite`] listing the
    /// knocker); an invite-only host surfaces it for a human to admit or
    /// deny. An older host drops the whole message — the knock simply
    /// goes unanswered, like knocking on a door nobody's behind.
    Knock,
    /// The host's "no" to a knock, so the asker isn't left waiting.
    Deny,
    /// Converge duplicate two-person rooms onto `into`. Either participant may
    /// send this, but receivers accept it only when both the old and replacement
    /// rosters are exactly that authenticated pair and `into` is the
    /// deterministic lowest room id. Carrying the replacement roster lets an
    /// offline peer repair a missed invite and still converge on reconnect.
    Merge {
        into: String,
        members: Vec<NodeId>,
        #[serde(default)]
        access: RoomAccess,
    },
    /// A member tells the **host** the files it's currently offering into
    /// the room's Shared Files area — replacement semantics (the member's
    /// full current list each time). The host aggregates every member's
    /// list and restates the whole as [`RoomEvent::Shares`]. The bytes
    /// never travel through the host: a downloader fetches them straight
    /// from the uploader over a `:shared` route. Sent member→host; ignored
    /// by anyone that isn't the room's host. An older host drops the whole
    /// message — its members simply see no shared files.
    ShareList { files: Vec<SharedFileMeta> },
    /// The **host's** authoritative Shared Files list for the room: every
    /// online member's offerings, each tagged with the uploader so a
    /// downloader knows whom to fetch from. Restated on every change and to
    /// each new joiner, exactly like the roster ([`RoomEvent::Invite`]);
    /// members ignore it from anyone but the host. Replacement semantics —
    /// an uploader that's gone offline simply drops off the next list.
    Shares { files: Vec<SharedEntry> },
    /// A room event a newer build introduced that this one doesn't know.
    /// Decodes here (the message still parses) and is ignored, instead of
    /// failing the whole [`RoomMessage`] — so one unknown event kind can't
    /// drop a roster or chat the receiver *does* understand.
    #[serde(other)]
    Unknown,
}

/// Point-to-point control traffic. Tagged on `t` so route, share,
/// ownership, site management, and app-level commands share one channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ControlMessage {
    Route(RouteControl),
    Share(ShareControl),
    Ownership(OwnershipControl),
    Site(SiteControl),
    App(AppControl),
    /// "Re-send me your presence." A peer's per-node *refresh* asks the target
    /// to re-announce its profile so the asker re-learns it on the spot — the
    /// guaranteed round-trip behind the refresh control (the receiver answers
    /// with an ordinary presence advert). The asker rate-limits these per
    /// target; an older peer drops the unknown `t` and the refresh simply falls
    /// back to re-pulling the daemon's cached view.
    ProfileRequest,
    Kvm(KvmControl),
    /// A control kind a newer build introduced that this one doesn't know.
    /// Decodes here and is ignored, so an unrecognised `t` can never fail the
    /// decode of the whole control channel — the route/share/ownership
    /// traffic this build *does* understand keeps flowing.
    #[serde(other)]
    Unknown,
}

/// One listening service on a machine, as reported to a co-owned fleet
/// member managing it remotely ([`SiteControl::Sites`]). Mirrors the scan's
/// `ListeningService` without the protocol crate depending on the inventory
/// crate — the backend fills it from a scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteService {
    pub id: String,
    pub name: String,
    pub port: u16,
    #[serde(default)]
    pub scheme: String,
    #[serde(default)]
    pub loopback: bool,
    #[serde(default)]
    pub process: String,
    /// The page `<title>` the probe fetched (http), a default-name hint.
    #[serde(default)]
    pub title: String,
}

/// Remotely managing a co-owned machine's sites — what powers the "Its
/// sites" controls in a fleet device's drawer. Authorized exactly like the
/// site proxy and the terminal: only the device's owner or a fleet member is
/// answered (the mesh authenticates the sender), so a stranger can't list or
/// re-expose your services. An older peer that doesn't know the `site` tag
/// drops the whole control message ([`ControlMessage`] fails to decode) —
/// the manager just sees no remote sites, never an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SiteControl {
    /// "List your sites" — a fleet member asking what this machine is
    /// listening on and what it currently exposes, to manage it.
    List,
    /// The answer: every discovered service, plus the current exposed map
    /// (id → advertised name).
    Sites {
        services: Vec<SiteService>,
        #[serde(default)]
        exposed: std::collections::BTreeMap<String, String>,
    },
    /// "Advertise exactly these" — the new exposed map (id → name) for this
    /// machine to publish. Applied only from the owner/fleet; the machine
    /// persists it and re-broadcasts presence.
    SetExposed {
        #[serde(default)]
        exposed: std::collections::BTreeMap<String, String>,
    },
    /// A site-management kind a newer build introduced. Ignored here rather
    /// than failing the enclosing [`ControlMessage`].
    #[serde(other)]
    Unknown,
}

/// App-level commands one of *your own* machines asks another to perform on
/// itself — things outside the route/share/ownership lifecycle. The receiver
/// enforces that the sender is its owner or a fleet co-member before acting
/// (the same rule that gates a terminal or remote-control session), so a
/// stranger on the mesh can never drive it. An older peer doesn't know the
/// `app` tag and drops the whole message, so a command simply goes
/// unanswered there — never misinterpreted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AppControl {
    /// Ask this machine to expose one user-selected folder as a native drive
    /// on the authenticated requester. The source validates/canonicalizes the
    /// path locally and accepts this only under the Files authorization gate
    /// (fleet, an explicit Files share, or active support consent).
    MapDrive {
        root: String,
        label: String,
        #[serde(default)]
        mount: String,
        request: String,
        /// Stable identity of the user's mapping intent. Route ids describe
        /// one live connection incarnation; this id survives reconnects so
        /// both affected machines can show and remove the same one-way map.
        #[serde(default)]
        mapping: String,
    },
    /// Open one **shared folder** on this machine as a native drive on the
    /// authenticated requester — the person-to-person twin of `MapDrive`.
    ///
    /// The difference is the whole point: `MapDrive` carries a `root`, which
    /// is fine because it is owner/fleet gated (naming a path on your own
    /// machine is no more than you could already do). A share reaches outside
    /// the fleet, so it carries **no path at all**. The requester names the
    /// minted `folder` id, and only the source's own registry can turn that
    /// back into a path — a receiver that could name a root could name `/`.
    ///
    /// The source accepts this only when it still holds a live share grant
    /// covering that folder's capability for this person, and only while the
    /// folder is still shared, so revoking either end closes it.
    ///
    /// `mount` is the *requester's* choice of where the drive appears on its
    /// own machine, exactly as for `MapDrive` — the sharer has no business
    /// picking a drive letter on someone else's desktop.
    MapFolder {
        folder: String,
        #[serde(default)]
        mount: String,
        request: String,
        #[serde(default)]
        mapping: String,
    },
    /// Ask one of the requester's own fleet machines to mount a folder that a
    /// third-party fleet shared with the requester. The destination asks the
    /// original source directly by opaque folder id, so the controller never
    /// proxies bytes and no source path appears at either fleet boundary.
    /// The normal owner/fleet app-control gate protects this command; the
    /// source independently verifies that the destination belongs to the
    /// fleet covered by the standing folder grant.
    MountSharedFolder {
        source: String,
        folder: String,
        #[serde(default)]
        mount: String,
    },
    /// One endpoint explicitly removed a native mapping. This is separate
    /// from route teardown: a route can disappear because a laptop slept,
    /// while removing the mapping is durable user intent shared by both ends.
    ForgetDrive { mapping: String },
    /// Confirmation that the other endpoint persisted the forget. Until this
    /// arrives, the requester retains and retries its tombstone when the peer
    /// returns, preventing an offline laptop from resurrecting an orphan map.
    ForgetDriveAck { mapping: String },
    /// The receiving OS finished mounting the mapping and reports the actual
    /// mount it selected (for example `Z:` when the request said Auto). The
    /// source stores this display-only mirror so every Drives surface tells
    /// the same story without turning the filesystem mapping bidirectional.
    DriveMounted {
        mapping: String,
        route: String,
        label: String,
        mount: String,
    },
    /// Ask one of *my own* machines to start sharing one of its folders, and
    /// report back the id it minted.
    ///
    /// The share builder lets you share any device you own, but a folder id
    /// can only be minted against the disk the folder is on — so picking a
    /// folder on another of your machines needs this round trip. Owner/fleet
    /// only (the default app-control gate): this says "share a folder of
    /// yours", which is a thing only its owner may say.
    ///
    /// A `path` here is not the leak `MapFolder` would be. It travels *inward*,
    /// from the owner to their own device, naming what to share — the opposite
    /// direction from a peer naming what it wants to open.
    ShareFolder {
        path: String,
        #[serde(default)]
        label: String,
        request: String,
    },
    /// The minted folder — the reply to [`AppControl::ShareFolder`]. The
    /// opaque request id lets the asker ignore anything it didn't ask for.
    ShareFolderResult {
        request: String,
        #[serde(default)]
        folder: String,
        #[serde(default)]
        label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Ask this authorized source machine to stage one local ISO/image (or a
    /// whole removable disk) on a KVM as BIOS-visible USB virtual media.
    StageKvmMedia {
        request: String,
        kvm: String,
        path: String,
        label: String,
    },
    /// The source accepted a KVM media request (`complete: false`) or reports
    /// its final mount result (`complete: true`). The opaque request id lets
    /// the viewer reject unsolicited/spoofed status messages.
    StageKvmMediaResult {
        request: String,
        complete: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// The computer physically attached to a KVM delegates its *current* CEC
    /// support technician to that appliance for a short, renewable window.
    /// The KVM accepts this only from its recorded `attached_to` node and
    /// expires it unless the customer's live control session keeps refreshing
    /// the heartbeat. This is what lets a technician reach the out-of-band
    /// power and virtual-media planes without turning support consent into
    /// unattended KVM access.
    KvmSupportGrant { technician: String, expires_in: u64 },
    /// Customer computer -> technician discovery half of
    /// [`AppControl::KvmSupportGrant`]. It tells the approved technician which
    /// KVM will shortly greet them on the support network, allowing that one
    /// targeted presence advert through the otherwise-strict CEC stranger
    /// filter. It has the same short renewable lifetime as the appliance grant.
    KvmSupportAvailable { kvm: NodeId, expires_in: u64 },
    /// "Update yourself and restart." Sent to a fleet machine running an
    /// AllMyStuff older than the channel's latest release. The receiver runs
    /// its self-updater and, if a newer build was applied, relaunches — its
    /// next presence advert (carrying the new [`NodeProfile::version`]) is
    /// the confirmation, exactly as a claim confirms by re-advertising its
    /// new owner.
    Upgrade,
    /// "Restart your AllMyStuff app." Sent to a fleet machine to relaunch its
    /// node onto the same build — the recovery step heavier than a reconnect
    /// but lighter than an upgrade (it stages/applies nothing). The receiver
    /// gates it owner/fleet, exactly like [`AppControl::Upgrade`], and relaunches
    /// the same OS-aware way; its next presence advert is the confirmation.
    Restart,
    /// "Reboot the machine you run on." The recovery step heavier than
    /// [`AppControl::Restart`] — the whole OS goes down and back, for the
    /// wedge an app relaunch can't clear (a hung compositor, a driver, a
    /// machine that needs its updates applied). Gated owner/fleet exactly
    /// like the app restart; the receiver hands it to the OS (`shutdown -r`
    /// and kin), so the actual reboot also answers to the OS's own privilege
    /// rules. Its presence dropping and returning is the confirmation. An
    /// older receiver decodes this as [`AppControl::Unknown`] and ignores it.
    RestartDevice,
    /// An app-level command a newer build introduced. Ignored here rather
    /// than failing the enclosing [`ControlMessage`].
    #[serde(other)]
    Unknown,
}

/// A KVM appliance's binding, carried in [`NodeProfile::kvm`] when the node
/// advertises [`FEATURE_KVM`]. A KVM is wired (HDMI + USB) into one machine
/// at a time; this says which graph node that is, so the UI can show "this
/// KVM controls *that* machine" and offer the target's screen/keyboard
/// through the KVM even when the target's own AllMyStuff agent is down (a
/// crashed OS, a BIOS screen, a headless box). The owner curates the binding
/// with [`KvmControl`]; the KVM is the source of truth and re-advertises on
/// every change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct KvmAdvert {
    /// The graph node this KVM is currently attached to — the machine it
    /// physically controls. `None` = not attached to anything yet (freshly
    /// claimed but never pointed at a target, or just detached). `None`
    /// serializes *without* the key, so an older receiver sees the unchanged
    /// shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attached_to: Option<NodeId>,
    /// The [`SiteAdvert::id`] (one of [`NodeProfile::sites`]) that serves the
    /// KVM's own web UI — the "Open KVM" button maps and opens this. Empty
    /// when the KVM hasn't named one; the UI then falls back to the first
    /// web-scheme site. Empty serializes *without* the key.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub web: String,
    /// The KVM's own **joining mesh** — the per-device `cec-kvm-xxxxx-xxxxx`
    /// network it ships with and returns to whenever it's reset/unclaimed.
    /// Derived deterministically from the device's identity, so it's the same
    /// name the KVM shows on its screen and web UI. The UI surfaces it so an
    /// owner always knows where a device will reappear after an unclaim —
    /// nothing is printed on a box. Empty (older KVM firmware) serializes
    /// *without* the key.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub joining_mesh: String,
    /// Every mesh (network id) the KVM is currently joined to, fleet included —
    /// the membership list a fleet owner curates with [`KvmControl::MeshAdd`] /
    /// [`KvmControl::MeshRemove`]. The KVM is the source of truth and
    /// re-advertises on every change, exactly like `attached_to`. Empty (older
    /// firmware, or genuinely none known yet) serializes *without* the key.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub meshes: Vec<String>,
    /// BIOS-visible USB media currently staged and mounted by this KVM.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub virtual_media: Option<KvmVirtualMediaAdvert>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KvmVirtualMediaAdvert {
    pub source: NodeId,
    pub label: String,
    pub file: String,
}

/// Curating a KVM appliance — its attachment binding and its mesh membership.
/// Authorized exactly like a terminal or a site route: only the device's owner
/// or a fleet co-member is obeyed (the mesh authenticates the sender), so a
/// stranger can't re-point your KVM or walk it onto a mesh. The KVM applies
/// the change, persists it, and re-advertises presence with the new
/// [`NodeProfile::kvm`] — that presence is the authoritative confirmation,
/// exactly as a claim confirms by re-advertising its new owner. An older peer
/// doesn't know the `kvm` tag and drops the whole [`ControlMessage`], so the
/// command simply goes unanswered there — never misinterpreted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum KvmControl {
    /// "Point this KVM at `node`." Binds the KVM to the target machine it
    /// controls. Re-attaching to a different node just replaces the binding.
    Attach {
        node: NodeId,
        /// The target's display label at attach time, so the KVM can rename
        /// itself `KVM-<label>` on both its AllMyStuff presence and its
        /// myownmesh identity — the graph then reads "KVM-den-tower controls
        /// den-tower" without the viewer resolving ids. Best-effort and
        /// cosmetic: empty (an older sender, or a target with no label yet)
        /// serializes *without* the key and the KVM falls back to the target
        /// node id. Refreshed on every re-attach, never live-tracked.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        label: String,
    },
    /// "This KVM no longer represents anything." Clears the binding. This is
    /// the deliberately-buried, confirm-gated action in the UI — detaching a
    /// KVM strips a machine of its out-of-band screen/keyboard, so it never
    /// rides the quick graph drawer.
    Detach,
    /// "Join this mesh too." Adds `network_id` to the KVM's mesh memberships
    /// ([`KvmAdvert::meshes`]) — the fleet-owner tool for walking a device
    /// onto another venue (a site mesh, a lab mesh) without touching its
    /// console. The KVM normalizes and validates the id, refuses its own
    /// fleet mesh (that membership is governed by the fleet key, never by
    /// this command), and re-advertises the new list.
    MeshAdd { network_id: String },
    /// "Leave this mesh." Removes `network_id` from the KVM's memberships and
    /// purges its local state for it. The fleet mesh is refused — evicting a
    /// device from its fleet is [`OwnershipControl::Release`] + governance,
    /// not a membership edit.
    MeshRemove { network_id: String },
    /// A KVM-control kind a newer build introduced. Ignored here rather than
    /// failing the enclosing [`ControlMessage`].
    #[serde(other)]
    Unknown,
}

/// One open terminal session a host advertises in answer to a
/// [`RouteControl::TerminalSessionsRequest`] — the row shape the viewer's
/// session picker renders so a fleet member (or another window of this very
/// machine) can discover and attach to a *shared* shell instead of always
/// minting a new one. Mirrors the host engine's `SessionInfo` without the
/// node crate's `terminal` types leaking into the protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSessionInfo {
    /// The host-side session id an [`RouteControl::Offer::session`] names to
    /// attach (`term-N`, or whatever the host minted).
    pub session_id: String,
    /// Friendly title (the shell's, falling back to the session id).
    pub title: String,
    /// Unix seconds the session was created — the picker shows its age.
    pub created_unix: u64,
    /// How many viewers are currently attached — `> 1` means already shared.
    pub attachers: usize,
}

/// Receiver-side OS mount requested for a Storage route. The canonical source
/// path is included so the receiver can re-request the mapping after either
/// app restarts. Drive pushes are fleet-only, and pull recipients supplied the
/// path themselves, so it is never disclosed outside an already-authorized
/// native-drive relationship.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriveRouteOffer {
    pub label: String,
    /// Stable user-intent id shared by the source and receiver. Optional for
    /// rolling compatibility; peers without it fall back to the route id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mapping: Option<String>,
    /// Canonical path on the source. Absent on older senders; those mappings
    /// remain live-only and cannot be rebuilt by the receiver after restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    /// Windows: `X:`. Other desktop platforms may use an absolute mount point.
    /// Empty means pick the next available native target on the receiver.
    #[serde(default)]
    pub mount: String,
    /// Opaque receiver-minted token for a pull the receiver explicitly
    /// requested. Absent means an unsolicited push, which is fleet-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<String>,
}

/// Lifecycle of a single cross-node route. The sourcing side offers; the
/// other side accepts to start media flowing, or rejects with a reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RouteControl {
    /// "I'd like to connect this." Carries the full route so the receiver
    /// can show exactly what's being asked and check it against its own
    /// catalog before accepting.
    Offer {
        route: Route,
        /// Video transports the *offerer* can consume for a display
        /// route, best first (today: `"h264"` — the mesh's RTP track
        /// lane). The accepting side — the machine whose screen will
        /// stream — picks the best one it can produce, falling back to
        /// MJPEG over the media channel when the list is empty or
        /// nothing matches. Absent on v0.1.x offers (`default`) and
        /// ignored by v0.1.x receivers: both skews degrade to MJPEG,
        /// never to a broken stream.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        video: Vec<String>,
        /// Audio transports the *offerer* can consume for an audio
        /// route, best first (today: `"opus"` — the mesh's RTP audio
        /// lane). Same degradation contract as `video`: absent or
        /// unrecognized on either side means PCM frames over the media
        /// channel, never a broken stream. Only meaningful when the
        /// offerer is the route's sink (the console's listen leg).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        audio: Vec<String>,
        /// For a **terminal** route: the host-side terminal session this
        /// viewer wants to attach to — `Some(id)` joins that already-running
        /// shell (tmux-style multi-attach: it shares the shell, the
        /// scrollback, and the keyboard with whoever else is attached),
        /// `None` mints a fresh shell. Meaningless on every other media
        /// kind. Absent on older offers (`default`) decodes as `None`
        /// (always a new shell — exactly v1's behaviour), and `None`
        /// serializes *without* the key so an older host sees the offer
        /// shape it always did.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
        /// Native drive mapping metadata for a Storage route. Absent for every
        /// other route and for peers predating native mapped drives.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        drive: Option<DriveRouteOffer>,
        /// A transient virtual-room scope. The receiving node accepts this
        /// only while its own UI has the same room joined and lists the
        /// authenticated sender as a member. It never becomes a durable share
        /// grant, and absent keeps ordinary/older route offers unchanged.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        room: Option<String>,
    },
    /// "Go ahead" — media may start. For a terminal route the host echoes
    /// the **resolved** session id it attached this route to (the minted
    /// `term-N` for a new shell, or the existing id for an attach), so the
    /// viewer can show "shared with N" and re-attach later. Absent on a
    /// non-terminal accept, or from an older host (`default` → `None`).
    Accept {
        route_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
        /// The streaming side selected paced-video v1 for this route. This is
        /// the acknowledgement half of capability negotiation: a viewer does
        /// not interpret same-timestamp samples as fragments merely because it
        /// advertised support; it does so only when the host's activating
        /// Accept says it will send that framing. Absent/false is the legacy
        /// whole-access-unit contract.
        #[serde(default, skip_serializing_if = "is_false")]
        paced_video: bool,
    },
    /// "No" — with a human reason ("not authorized", "device busy").
    Reject { route_id: String, reason: String },
    /// "Stop" — either side can tear a live route down.
    Teardown { route_id: String },
    /// "Give me a clean decode entry *now*" — the viewer's decoder lost
    /// its place (a decode error, a rebuilt decoder) and shouldn't sit
    /// out the rest of the periodic IDR interval. The streaming side
    /// forces an IDR on its next capture. Unknown to v0.2.x peers (the
    /// whole message fails their decode and is dropped): recovery then
    /// simply waits for the periodic IDR, as before.
    Refresh { route_id: String },
    /// "Stream with these settings" — the viewer's quality picks for a
    /// display route it consumes; the streaming side restarts its
    /// capture with the overrides. `None` everywhere = automatic (the
    /// streamer's own budget). Unknown to v0.2.x peers and dropped,
    /// leaving their stream on automatic.
    Tune {
        route_id: String,
        /// Longest output edge in pixels (e.g. 1920); `None` = native.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_edge: Option<u32>,
        /// H.264 target bitrate in bits/second; `None` = pixel-budgeted.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bitrate: Option<u32>,
        /// Capture rate ceiling; `None` = the streamer's default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fps: Option<u32>,
        /// Game mode: the latency-first posture (GDR intra-refresh on
        /// capable streamers, 60 fps floor off-LAN, tight burst bounds).
        /// Defaults off (balanced) and deserializes absent as off, so
        /// peers that predate it interoperate untouched.
        #[serde(default)]
        game: bool,
        /// The stream posture by name — `"balanced"`, `"game"`, or
        /// `"studio"` (the LAN fidelity mode: high-bitrate quality-first
        /// encoding). Wins over `game` when present; `game` stays for
        /// hosts that predate the tri-state. Absent = derive from `game`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<String>,
        /// **Opaque, encoder/decoder-backend-owned extension.** The
        /// protocol layer carries it verbatim and never inspects it — the
        /// node's video pipeline defines its shape and reads it end to
        /// end. This is the seam that keeps pipeline tuning
        /// backend-only: any future viewer-requested encoder knob rides
        /// here instead of adding a typed field to this crate. Absent for
        /// every peer that doesn't set it.
        #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
        ext: serde_json::Value,
    },
    /// "Here's how your stream is actually arriving" — the viewer reports its
    /// decode health back to the streamer, periodically, so the streamer can
    /// adapt (recovery cadence today; bitrate/resolution auto-scaling next).
    /// All counters are *since the last report*. The reverse of every other
    /// route message: it flows receiver → sender. Unknown to v0.2.x peers (it
    /// decodes as [`RouteControl::Unknown`] and is dropped), so an older
    /// streamer simply never adapts — exactly today's behaviour.
    VideoFeedback {
        route_id: String,
        /// Frames the viewer actually rendered per second over the window —
        /// compare to the stream's target to spot a struggling link.
        #[serde(default)]
        recv_fps: u32,
        /// Decode failures since the last report (lost/!corrupt access units,
        /// rebuilt decoders) — the headline "this link is lossy" signal.
        #[serde(default)]
        decode_fails: u32,
        /// How deep the viewer's decode queue is backed up (0 = keeping up;
        /// large = the viewer can't drain as fast as frames arrive).
        #[serde(default)]
        queue_depth: u32,
        /// Frame health: the presentation timestamp (µs) of the most
        /// recent access unit that failed to decode, when one did — lets
        /// a capable host heal with a targeted refresh (GDR wave restart
        /// today, reference invalidation next) instead of a keyframe
        /// wall. Old peers ignore it.
        #[serde(default)]
        lost_ts_us: Option<u64>,
        /// **Opaque, decoder/encoder-backend-owned extension.** The
        /// protocol layer relays it verbatim and never inspects it — the
        /// node's video pipeline defines its shape (bandwidth estimate,
        /// delay trend, and any future receiver-side signal) and reads it
        /// end to end. The seam that keeps pipeline tuning backend-only:
        /// a new feedback metric adds a field to the node's own struct,
        /// not a typed field here. Absent for peers that don't set it.
        #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
        ext: serde_json::Value,
    },
    /// "Your inbound video for this route rides track lane N." The streaming
    /// (host) side tells the viewer which RTP track lane it pinned a
    /// display/camera route to, so the viewer demuxes inbound H.264 by this
    /// explicit binding instead of inferring it from a positional sort the two
    /// ends can briefly disagree on while routes come and go — the
    /// disagreement that flashed one monitor's frames in another monitor's
    /// window when several feeds were open. The lane is pinned for the route's
    /// lifetime, so this is sent once when the stream starts. Unknown to older
    /// peers (decodes as [`RouteControl::Unknown`] and dropped): they fall back
    /// to the positional lane, exactly as before.
    VideoLane { route_id: String, lane: u8 },
    /// "Your media keeps arriving on my track lane N but no route here maps
    /// to it" — receiver → sender, the lane-shaped twin of a
    /// [`Reject`](RouteControl::Reject). It exists for the one failure a
    /// Reject can't express: the receiving app restarted (or otherwise lost
    /// its route table), so it can't *name* the dead route — the name is
    /// exactly what it lost — while the sender keeps capturing and encoding
    /// into the void. Only the sender still knows which route it pinned to
    /// that lane, so the receiver reports the lane and the sender resolves
    /// it, then treats the result as a Reject of that route (stopping the
    /// encoder). `media` is `"video"` (the H.264 track lanes) or `"audio"`
    /// (the Opus lanes); other values are reserved and ignored. Unknown to
    /// older peers (decodes as [`RouteControl::Unknown`] and dropped) — an
    /// old sender keeps streaming exactly as today.
    DeadLane { media: String, lane: u8 },
    /// "List your open terminal sessions" — a viewer asking a host (its
    /// owner/fleet, enforced host-side exactly like a terminal offer) which
    /// shells it already has running, so the picker can offer to *attach* to
    /// one instead of always spawning a new shell. An older host doesn't
    /// know the kind and drops it (it decodes as [`RouteControl::Unknown`]
    /// rather than failing the control channel) — the picker simply shows no
    /// existing sessions, never an error.
    TerminalSessionsRequest,
    /// The host's answer to [`TerminalSessionsRequest`]: every terminal
    /// session it currently has open, for the viewer's picker. Each row
    /// names the `session_id` an [`Offer::session`](RouteControl::Offer)
    /// attaches to. An older viewer drops this (it decodes as `Unknown`).
    TerminalSessions { sessions: Vec<TerminalSessionInfo> },
    /// A route-control kind a newer build introduced (a future negotiation
    /// step). Decodes here and is ignored, so it can't fail the whole
    /// [`ControlMessage`] and take the live route handshake — `Offer` /
    /// `Accept` / `Teardown` — down with it.
    #[serde(other)]
    Unknown,
}

/// Negotiating a *shared* relationship and its grants. This is how the
/// "is this someone I'm sharing with?" answer becomes a mutual fact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ShareControl {
    /// "I'd like to share some things with you." Offered grants describe
    /// what the inviter is willing to let the invitee do.
    Invite {
        from: Person,
        #[serde(default)]
        grants: Vec<Grant>,
    },
    /// The invitee accepts the share (optionally proposing grants back, so
    /// sharing can be mutual in one round trip).
    Accept {
        #[serde(default)]
        grants: Vec<Grant>,
        /// The invitee's **fleet siblings** — the other devices a share with
        /// this person is meant to reach.
        ///
        /// A share is one machine of mine extended to a whole fleet, but the
        /// invite only ever reaches the one device the sharer picked, and the
        /// sharer authorizes by *node*. Without this the share works from that
        /// device and is refused from every other machine of theirs.
        ///
        /// This is a **vouch, not an assertion**: the sharer binds these only
        /// into the share the authenticated sender already belongs to. A device
        /// can't add itself, and a stranger's list binds nothing — only a peer
        /// already inside a share can widen it, and only to that share.
        /// Absent from an older peer, which simply keeps the one-device
        /// behaviour.
        #[serde(default)]
        fleet: Vec<NodeId>,
    },
    /// A fleet sibling relaying a share **our** fleet was granted, so every
    /// device knows about it — not just the one that happened to accept.
    ///
    /// Accepted only from this device's owner or a signed-roster fleet member;
    /// from anyone else it's ignored, so nobody can talk a machine into
    /// believing it holds a share. Safe by construction even so: these are
    /// *inbound* grants, which only ever widen what we may pull from them, and
    /// the far side enforces its own grants regardless.
    FleetShare {
        /// The device that extended the grants. The person is derived from
        /// this — never from a self-asserted id — exactly as an inbound invite
        /// is keyed by its authenticated sender.
        source: NodeId,
        /// Their display name. UI only.
        #[serde(default)]
        name: String,
        #[serde(default)]
        grants: Vec<Grant>,
    },
    /// The invitee declines.
    Decline,
    /// Either side withdraws a previously-granted permission. Revocation
    /// is unilateral — you can always take back your own stuff.
    Revoke { grant_id: String },
    /// A share-control kind a newer build introduced. Ignored here rather
    /// than failing the enclosing [`ControlMessage`].
    #[serde(other)]
    Unknown,
}

/// Adopting an unowned device that's been put up for adoption. Ownership is
/// the answer to "whose machine is this?" — and unlike a share, you can't
/// assert it unilaterally: the device must be in claim mode (advertising
/// [`NodeProfile::claimable`]) for a claim to take. This keeps a stranger
/// on the mesh from flat-out grabbing a box that already has an owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OwnershipControl {
    /// "I'm adopting you." Sent to a claimable device; `owner` is the
    /// claimer's node id. The device records it (if still claimable),
    /// stops being claimable, and re-advertises presence with the new
    /// owner — that presence is the authoritative confirmation.
    Claim { owner: NodeId },
    /// The device confirms the adoption took — it's now owned by `owner`.
    Claimed { owner: NodeId },
    /// The device refuses: it isn't in claim mode, or already has an owner.
    Declined { reason: String },
    /// The owner relinquishes the device, returning it to unowned. Only the
    /// current owner's release is honoured.
    Release,
    /// The owner hands the freshly-claimed device its fleet credential: the
    /// shared fleet key (from which both sides derive the same closed-network
    /// id) and the fleet's display name. Sent point-to-point right after the
    /// claim is confirmed — it replaces the old gossiped `OwnedRoster`. The
    /// device adopts the key, joins the fleet's closed network, and converges
    /// its signed roster from the owner's governance.
    ///
    /// `venue` carries the owner's fleet-network **transport config** (its
    /// signaling / STUN / TURN servers) as a JSON object string, so a joining
    /// member calls out where the rest of the fleet does instead of at its own
    /// default. The venue is owner-defined: only the fleet owner sets it, and a
    /// change re-hands the key to broadcast it. Carried as a string (not a typed
    /// value) so the message stays `Eq` and preserves every config field
    /// verbatim. `None` from an older owner, or before any venue is configured.
    FleetKey {
        key: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        venue: Option<String>,
    },
    /// A member tells its owner it's leaving the fleet, so the owner removes
    /// it from the signed roster (a propagating evict) instead of believing
    /// it's still a member. Sent by the leaver to its owner just before it
    /// drops the fleet network; the owner reconciles its roster on receipt.
    FleetDeparted,
    /// An ownership kind a newer build introduced. Ignored here rather than
    /// failing the enclosing [`ControlMessage`].
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;
    use allmystuff_graph::{Flow, GrantRole, MediaKind};

    /// RFC 4648 base32 vector ("hello" → NBSWY3DP), lowercased — the
    /// hand-rolled encoder must match what NanoKVM's Go stdlib
    /// produces for the same bytes. FROZEN.
    #[test]
    fn claim_code_encoding_matches_rfc4648() {
        assert_eq!(claim_code_from_bytes(b"hello"), "nbswy3dp");
        // A full-size code: 16 bytes → 26 chars, alphabet-only.
        let code = claim_code_from_bytes(&[0xAB; CLAIM_CODE_BYTES]);
        assert_eq!(code.len(), 26);
        assert!(code
            .chars()
            .all(|c| c.is_ascii_lowercase() || ('2'..='7').contains(&c)));
        // Deterministic.
        assert_eq!(code, claim_code_from_bytes(&[0xAB; CLAIM_CODE_BYTES]));
    }

    /// Display formatting is cosmetic and must normalize away in the
    /// network-id derivation — claimer types the dashed form, claimee
    /// derived from the raw form, both land in the same room. FROZEN.
    #[test]
    fn claim_code_network_id_normalizes_display_forms() {
        let code = claim_code_from_bytes(&[7; CLAIM_CODE_BYTES]);
        let pretty = format_claim_code(&code);
        assert!(pretty.contains('-'));
        assert_eq!(claim_code_network_id(&pretty), claim_code_network_id(&code));
        assert_eq!(
            claim_code_network_id(&pretty.to_uppercase()),
            format!("amsclaim-{code}")
        );
    }

    #[test]
    fn local_claim_network_id_is_frozen() {
        // Load-bearing constant mirrored by NanoKVM's Go bridge —
        // changing it strands shipped devices on the old rendezvous.
        assert_eq!(LOCAL_CLAIM_NETWORK_ID, "allmystuff-local-claim-v1");
    }

    #[test]
    fn node_profile_round_trips() {
        let p = NodeProfile {
            protocol: PROTOCOL_VERSION,
            node: "desk".into(),
            label: "Desk PC".into(),
            hostname: "desk-pc.local".into(),
            summary: InventorySummary {
                os: "linux".into(),
                cpu: "Test CPU".into(),
                ram_bytes: 16 << 30,
                device_count: 12,
                product: "OptiPlex 7090".into(),
            },
            capabilities: vec![Capability::new(
                "desk",
                "desk:mic",
                "Mic",
                MediaKind::Audio,
                Flow::Source,
                "microphone",
            )],
            owner: Some("my-laptop".into()),
            claimable: false,
            boot: 7,
            features: vec![FEATURE_TERMINAL.into()],
            sites: vec![SiteAdvert {
                id: "tcp:8080".into(),
                label: "HTTP".into(),
                port: 8080,
                scheme: "http".into(),
                loopback: true,
            }],
            version: "0.1.11".into(),
            fleet_name: "Casey".into(),
            fleet_owner: "Casey".into(),
            kvm: None,
            sent_at: 1_751_400_000_000,
        };
        let s = serde_json::to_string(&p).unwrap();
        let back: NodeProfile = serde_json::from_str(&s).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn presence_sent_at_accepts_skew_both_ways() {
        // An older peer's advert has no `sent_at` — it decodes as 0 ("no
        // sample") rather than failing, so the node never vanishes and the
        // skew estimator simply skips it.
        let json = r#"{
            "protocol": 1, "node": "old", "label": "Old", "hostname": "old",
            "summary": {"os":"linux","cpu":"cpu","ram_bytes":1,"device_count":1}
        }"#;
        let p: NodeProfile = serde_json::from_str(json).unwrap();
        assert_eq!(p.sent_at, 0);

        // Zero serializes *without* the key, so an older receiver — and the
        // KVM bridge's pinned Go fixtures — sees the unchanged shape.
        let s = serde_json::to_string(&p).unwrap();
        assert!(!s.contains("sent_at"));

        // A populated stamp round-trips.
        let p = NodeProfile {
            sent_at: 1_751_400_000_000,
            ..p
        };
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains("\"sent_at\":1751400000000"));
        let back: NodeProfile = serde_json::from_str(&s).unwrap();
        assert_eq!(back.sent_at, 1_751_400_000_000);
    }

    #[test]
    fn presence_fleet_metadata_accepts_skew_both_ways() {
        // An older peer's advert has no `fleet_name` / `fleet_owner` — they
        // decode as empty rather than failing, so the node never vanishes from
        // the graph (it just isn't grouped into a named fleet).
        let json = r#"{
            "protocol": 1, "node": "old", "label": "Old", "hostname": "old",
            "summary": {"os":"linux","cpu":"cpu","ram_bytes":1,"device_count":1}
        }"#;
        let p: NodeProfile = serde_json::from_str(json).unwrap();
        assert!(p.fleet_name.is_empty());
        assert!(p.fleet_owner.is_empty());

        // Empty fleet metadata serializes *without* the keys, so an older
        // receiver sees exactly the presence shape it always did.
        let s = serde_json::to_string(&p).unwrap();
        assert!(!s.contains("fleet_name"));
        assert!(!s.contains("fleet_owner"));

        // Populated values round-trip and carry the owner's *person* name.
        let p = NodeProfile {
            fleet_name: "Casey".into(),
            fleet_owner: "Casey".into(),
            ..p
        };
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains("\"fleet_name\":\"Casey\""));
        assert!(s.contains("\"fleet_owner\":\"Casey\""));
        let back: NodeProfile = serde_json::from_str(&s).unwrap();
        assert_eq!(back.fleet_name, "Casey");
        assert_eq!(back.fleet_owner, "Casey");
    }

    #[test]
    fn presence_sites_accept_skew_both_ways() {
        // An older peer's advert has no `sites` — it decodes as empty
        // rather than failing (the node never vanishes from the graph).
        let json = r#"{
            "protocol": 1, "node": "old", "label": "Old", "hostname": "old",
            "summary": {"os":"linux","cpu":"cpu","ram_bytes":1,"device_count":1}
        }"#;
        let p: NodeProfile = serde_json::from_str(json).unwrap();
        assert!(p.sites.is_empty());

        // No sites serializes *without* the key, so an older receiver sees
        // exactly the presence shape it always did.
        let s = serde_json::to_string(&p).unwrap();
        assert!(!s.contains("sites"));

        // A populated list round-trips, and `is_web` keys on the scheme.
        let p = NodeProfile {
            sites: vec![
                SiteAdvert {
                    id: "tcp:5432".into(),
                    label: "PostgreSQL".into(),
                    port: 5432,
                    scheme: "postgres".into(),
                    loopback: true,
                },
                SiteAdvert {
                    id: "tcp:443".into(),
                    label: "HTTPS".into(),
                    port: 443,
                    scheme: "https".into(),
                    loopback: false,
                },
            ],
            ..p
        };
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains("\"sites\""));
        let back: NodeProfile = serde_json::from_str(&s).unwrap();
        assert_eq!(back.sites, p.sites);
        assert!(!back.sites[0].is_web(), "postgres isn't web");
        assert!(back.sites[1].is_web(), "https is web");
    }

    #[test]
    fn presence_version_accepts_skew_both_ways() {
        // An older peer's advert has no `version` — it decodes as empty
        // ("unknown") rather than failing, so the node never vanishes.
        let json = r#"{
            "protocol": 1, "node": "old", "label": "Old", "hostname": "old",
            "summary": {"os":"linux","cpu":"cpu","ram_bytes":1,"device_count":1}
        }"#;
        let p: NodeProfile = serde_json::from_str(json).unwrap();
        assert!(p.version.is_empty());

        // Empty version serializes *without* the key, so an older receiver
        // sees exactly the presence shape it always did.
        let s = serde_json::to_string(&p).unwrap();
        assert!(!s.contains("version"));

        // A populated version round-trips.
        let p = NodeProfile {
            version: "0.2.0".into(),
            ..p
        };
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains("\"version\":\"0.2.0\""));
        let back: NodeProfile = serde_json::from_str(&s).unwrap();
        assert_eq!(back.version, "0.2.0");
    }

    #[test]
    fn app_control_upgrade_round_trips() {
        let msg = ControlMessage::App(AppControl::Upgrade);
        let s = serde_json::to_string(&msg).unwrap();
        // Tagged `t: "app"` at the outer level, `kind: "upgrade"` within.
        assert!(s.contains("\"t\":\"app\""));
        assert!(s.contains("\"kind\":\"upgrade\""));
        let back: ControlMessage = serde_json::from_str(&s).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn drive_mapping_identity_and_lifecycle_round_trip() {
        let request = ControlMessage::App(AppControl::MapDrive {
            root: "C:\\Users\\Tracy\\Documents".into(),
            label: "Laptop Documents".into(),
            mount: "Z:".into(),
            request: "request-1".into(),
            mapping: "mapping-1".into(),
        });
        let encoded = serde_json::to_string(&request).unwrap();
        assert!(encoded.contains("\"mapping\":\"mapping-1\""));
        assert_eq!(
            serde_json::from_str::<ControlMessage>(&encoded).unwrap(),
            request
        );

        for message in [
            AppControl::DriveMounted {
                mapping: "mapping-1".into(),
                route: "route-1".into(),
                label: "Laptop Documents".into(),
                mount: "Z:".into(),
            },
            AppControl::ForgetDrive {
                mapping: "mapping-1".into(),
            },
            AppControl::ForgetDriveAck {
                mapping: "mapping-1".into(),
            },
        ] {
            let message = ControlMessage::App(message);
            let encoded = serde_json::to_string(&message).unwrap();
            assert_eq!(
                serde_json::from_str::<ControlMessage>(&encoded).unwrap(),
                message
            );
        }
    }

    #[test]
    fn drive_mapping_identity_accepts_older_requests_and_offers() {
        let old_request = serde_json::json!({
            "t": "app",
            "kind": "map_drive",
            "root": "C:\\Work",
            "label": "Work",
            "mount": "W:",
            "request": "old-request"
        });
        let decoded: ControlMessage = serde_json::from_value(old_request).unwrap();
        assert!(matches!(
            decoded,
            ControlMessage::App(AppControl::MapDrive { mapping, .. }) if mapping.is_empty()
        ));

        let old_offer = serde_json::json!({
            "label": "Work",
            "root": "C:\\Work",
            "mount": "W:"
        });
        let decoded: DriveRouteOffer = serde_json::from_value(old_offer).unwrap();
        assert_eq!(decoded.mapping, None);
    }

    #[test]
    fn fleet_shared_folder_mount_round_trips_without_a_source_path() {
        let msg = ControlMessage::App(AppControl::MountSharedFolder {
            source: "outside-fleet-node".into(),
            folder: "opaque-folder-id".into(),
            mount: "X:".into(),
        });
        let encoded = serde_json::to_string(&msg).unwrap();
        assert!(encoded.contains("\"kind\":\"mount_shared_folder\""));
        assert!(!encoded.contains("source_path"));
        assert_eq!(
            serde_json::from_str::<ControlMessage>(&encoded).unwrap(),
            msg
        );
    }

    #[test]
    fn kvm_media_control_round_trips_request_and_result() {
        let request = ControlMessage::App(AppControl::StageKvmMedia {
            request: "opaque".into(),
            kvm: "kvm".into(),
            path: "/tmp/windows.iso".into(),
            label: "Windows".into(),
        });
        let encoded = serde_json::to_string(&request).unwrap();
        assert!(encoded.contains("\"kind\":\"stage_kvm_media\""));
        assert_eq!(
            serde_json::from_str::<ControlMessage>(&encoded).unwrap(),
            request
        );

        let result = ControlMessage::App(AppControl::StageKvmMediaResult {
            request: "opaque".into(),
            complete: true,
            error: Some("upload failed".into()),
        });
        let encoded = serde_json::to_string(&result).unwrap();
        assert!(encoded.contains("\"kind\":\"stage_kvm_media_result\""));
        assert_eq!(
            serde_json::from_str::<ControlMessage>(&encoded).unwrap(),
            result
        );
    }

    #[test]
    fn kvm_support_passthrough_controls_round_trip() {
        let grant = ControlMessage::App(AppControl::KvmSupportGrant {
            technician: "tech".into(),
            expires_in: 8,
        });
        let encoded = serde_json::to_string(&grant).unwrap();
        assert!(encoded.contains("\"kind\":\"kvm_support_grant\""));
        assert_eq!(
            serde_json::from_str::<ControlMessage>(&encoded).unwrap(),
            grant
        );

        let available = ControlMessage::App(AppControl::KvmSupportAvailable {
            kvm: NodeId::from("kvm"),
            expires_in: 8,
        });
        let encoded = serde_json::to_string(&available).unwrap();
        assert!(encoded.contains("\"kind\":\"kvm_support_available\""));
        assert_eq!(
            serde_json::from_str::<ControlMessage>(&encoded).unwrap(),
            available
        );
    }

    #[test]
    fn kvm_control_attach_detach_round_trip_and_tag() {
        // Attach is tagged `t: "kvm"` outer, `kind: "attach"` within, carrying
        // the target node it binds the KVM to.
        let attach = ControlMessage::Kvm(KvmControl::Attach {
            node: "den-tower".into(),
            label: String::new(),
        });
        let s = serde_json::to_string(&attach).unwrap();
        assert!(s.contains("\"t\":\"kvm\""));
        assert!(s.contains("\"kind\":\"attach\""));
        assert!(s.contains("\"node\":\"den-tower\""));
        assert_eq!(serde_json::from_str::<ControlMessage>(&s).unwrap(), attach);

        let detach = ControlMessage::Kvm(KvmControl::Detach);
        let s = serde_json::to_string(&detach).unwrap();
        assert!(s.contains("\"kind\":\"detach\""));
        assert_eq!(serde_json::from_str::<ControlMessage>(&s).unwrap(), detach);
    }

    #[test]
    fn old_peer_drops_kvm_control_to_unknown() {
        // A build that never heard of the `kvm` tag must decode the whole
        // message as `ControlMessage::Unknown` — never fail the control
        // channel, so the route/share traffic it *does* know keeps flowing.
        let unknown = serde_json::json!({ "t": "kvm", "kind": "attach", "node": "x" });
        let m: ControlMessage =
            serde_json::from_value(unknown).expect("unknown outer tag must not error");
        // (This build *does* know `kvm`, so it decodes to the real variant; the
        // guarantee under test is that an *unknown nested kind* never errors.)
        let future = serde_json::json!({ "t": "kvm", "kind": "attach_to_two_at_once" });
        let m2: ControlMessage =
            serde_json::from_value(future).expect("unknown nested kind must not error");
        assert_eq!(m2, ControlMessage::Kvm(KvmControl::Unknown));
        assert!(matches!(m, ControlMessage::Kvm(KvmControl::Attach { .. })));
    }

    #[test]
    fn presence_kvm_accepts_skew_both_ways() {
        // An ordinary machine (or an older peer) has no `kvm` field — it
        // decodes as `None` rather than failing, so the node never vanishes.
        let json = r#"{
            "protocol": 1, "node": "old", "label": "Old", "hostname": "old",
            "summary": {"os":"linux","cpu":"cpu","ram_bytes":1,"device_count":1}
        }"#;
        let p: NodeProfile = serde_json::from_str(json).unwrap();
        assert_eq!(p.kvm, None);

        // `None` serializes *without* the key, so an older receiver sees the
        // unchanged presence shape.
        let s = serde_json::to_string(&p).unwrap();
        assert!(!s.contains("kvm"));

        // A KVM advertising an attachment + web site round-trips, and an empty
        // `web` is omitted on the wire.
        let p = NodeProfile {
            features: vec![FEATURE_KVM.into()],
            kvm: Some(KvmAdvert {
                attached_to: Some("den-tower".into()),
                web: "tcp:80".into(),
                ..Default::default()
            }),
            ..p
        };
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains("\"kvm\""));
        assert!(s.contains("\"attached_to\":\"den-tower\""));
        assert!(s.contains("\"web\":\"tcp:80\""));
        let back: NodeProfile = serde_json::from_str(&s).unwrap();
        assert_eq!(back, p);

        // A freshly-claimed-but-unattached KVM omits `attached_to`.
        let p = NodeProfile {
            kvm: Some(KvmAdvert::default()),
            ..p
        };
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains("\"kvm\":{}"));
        let back: NodeProfile = serde_json::from_str(&s).unwrap();
        assert_eq!(back.kvm.unwrap().attached_to, None);
    }

    #[test]
    fn kvm_advert_joining_mesh_and_meshes_accept_skew_both_ways() {
        // Older firmware's advert has neither field — both default (empty)
        // rather than failing, and empties serialize *without* the keys so an
        // older receiver sees the unchanged shape.
        let old = serde_json::json!({ "web": "tcp:80" });
        let a: KvmAdvert = serde_json::from_value(old).unwrap();
        assert_eq!(a.joining_mesh, "");
        assert!(a.meshes.is_empty());
        let s = serde_json::to_string(&a).unwrap();
        assert!(!s.contains("joining_mesh"));
        assert!(!s.contains("meshes"));

        // New firmware's advert round-trips both.
        let a = KvmAdvert {
            joining_mesh: "cec-kvm-ab3de-fg7hj".into(),
            meshes: vec!["amber-turing-x3k9q".into(), "cec-kvm-ab3de-fg7hj".into()],
            ..a
        };
        let s = serde_json::to_string(&a).unwrap();
        assert!(s.contains("\"joining_mesh\":\"cec-kvm-ab3de-fg7hj\""));
        assert!(s.contains("\"meshes\":[\"amber-turing-x3k9q\",\"cec-kvm-ab3de-fg7hj\"]"));
        let back: KvmAdvert = serde_json::from_str(&s).unwrap();
        assert_eq!(back, a);
    }

    #[test]
    fn kvm_control_mesh_add_remove_round_trip_and_tag() {
        // MeshAdd/MeshRemove ride the same `t: "kvm"` envelope as attach.
        let add = ControlMessage::Kvm(KvmControl::MeshAdd {
            network_id: "den-site-mesh".into(),
        });
        let s = serde_json::to_string(&add).unwrap();
        assert!(s.contains("\"t\":\"kvm\""));
        assert!(s.contains("\"kind\":\"mesh_add\""));
        assert!(s.contains("\"network_id\":\"den-site-mesh\""));
        assert_eq!(serde_json::from_str::<ControlMessage>(&s).unwrap(), add);

        let remove = ControlMessage::Kvm(KvmControl::MeshRemove {
            network_id: "den-site-mesh".into(),
        });
        let s = serde_json::to_string(&remove).unwrap();
        assert!(s.contains("\"kind\":\"mesh_remove\""));
        assert_eq!(serde_json::from_str::<ControlMessage>(&s).unwrap(), remove);
    }

    #[test]
    fn kvm_control_attach_label_accepts_skew_both_ways() {
        // An older sender's attach has no `label` — it decodes as empty
        // rather than failing, and an empty label serializes *without* the
        // key so an older KVM sees the exact attach shape it always did.
        let old = serde_json::json!({ "t": "kvm", "kind": "attach", "node": "den-tower" });
        let m: ControlMessage = serde_json::from_value(old).unwrap();
        assert_eq!(
            m,
            ControlMessage::Kvm(KvmControl::Attach {
                node: "den-tower".into(),
                label: String::new(),
            })
        );
        let s = serde_json::to_string(&m).unwrap();
        assert!(!s.contains("label"));

        // A labelled attach round-trips.
        let m = ControlMessage::Kvm(KvmControl::Attach {
            node: "den-tower".into(),
            label: "Den Tower".into(),
        });
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains("\"label\":\"Den Tower\""));
        assert_eq!(serde_json::from_str::<ControlMessage>(&s).unwrap(), m);
    }

    #[test]
    fn presence_features_accept_skew_both_ways() {
        // An older peer's advert has no `features` — it decodes as empty
        // rather than failing (so the node never vanishes from the graph).
        let json = r#"{
            "protocol": 1, "node": "old", "label": "Old", "hostname": "old",
            "summary": {"os":"linux","cpu":"cpu","ram_bytes":1,"device_count":1}
        }"#;
        let p: NodeProfile = serde_json::from_str(json).unwrap();
        assert!(p.features.is_empty());

        // Empty features serialize *without* the key, so an older receiver
        // sees exactly the presence shape it always did.
        let s = serde_json::to_string(&p).unwrap();
        assert!(!s.contains("features"));

        // A populated list round-trips.
        let p = NodeProfile {
            features: vec![FEATURE_TERMINAL.into()],
            ..p
        };
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains("\"features\":[\"terminal\"]"));
        let back: NodeProfile = serde_json::from_str(&s).unwrap();
        assert_eq!(back.features, vec![FEATURE_TERMINAL.to_string()]);
    }

    #[test]
    fn route_offer_video_accepts_skew_both_ways() {
        // A v0.1.x offer has no `video` field — it decodes as "MJPEG
        // only" rather than failing.
        let legacy = r#"{"kind":"offer","route":{
            "id":"r1","from":"a:screen","to":"b:view","media":"display"
        }}"#;
        let rc: RouteControl = serde_json::from_str(legacy).unwrap();
        assert!(matches!(rc, RouteControl::Offer { ref video, .. } if video.is_empty()));

        // An empty accepts list serializes *without* the field, so a
        // v0.1.x receiver sees exactly the shape it always did.
        let s = serde_json::to_string(&rc).unwrap();
        assert!(!s.contains("video"));

        // A populated list round-trips.
        let offered = match rc {
            RouteControl::Offer { route, .. } => RouteControl::Offer {
                route,
                video: vec!["h264".into()],
                audio: Vec::new(),
                session: None,
                drive: None,
                room: None,
            },
            _ => unreachable!(),
        };
        let s = serde_json::to_string(&offered).unwrap();
        assert!(s.contains("\"video\":[\"h264\"]"));
        let back: RouteControl = serde_json::from_str(&s).unwrap();
        assert_eq!(offered, back);
    }

    #[test]
    fn route_offer_room_scope_is_optional_and_round_trips() {
        let legacy = r#"{"kind":"offer","route":{
            "id":"r1","from":"a:screen","to":"b:view","media":"display"
        }}"#;
        let plain: RouteControl = serde_json::from_str(legacy).unwrap();
        assert!(matches!(plain, RouteControl::Offer { room: None, .. }));
        assert!(!serde_json::to_string(&plain).unwrap().contains("\"room\""));

        let scoped = match plain {
            RouteControl::Offer {
                route,
                video,
                audio,
                session,
                drive,
                ..
            } => RouteControl::Offer {
                route,
                video,
                audio,
                session,
                drive,
                room: Some("room:a:abc".into()),
            },
            _ => unreachable!(),
        };
        let json = serde_json::to_string(&scoped).unwrap();
        assert!(json.contains("\"room\":\"room:a:abc\""));
        assert_eq!(serde_json::from_str::<RouteControl>(&json).unwrap(), scoped);
    }

    #[test]
    fn route_offer_audio_accepts_skew_both_ways() {
        // The audio accepts ride the same contract as video's: absent
        // decodes as "PCM channel only", empty serializes invisibly,
        // populated round-trips.
        let legacy = r#"{"kind":"offer","route":{
            "id":"r1","from":"a:system-audio","to":"b:system-audio","media":"audio"
        }}"#;
        let rc: RouteControl = serde_json::from_str(legacy).unwrap();
        assert!(matches!(rc, RouteControl::Offer { ref audio, .. } if audio.is_empty()));
        let s = serde_json::to_string(&rc).unwrap();
        assert!(!s.contains("audio\":["));

        let offered = match rc {
            RouteControl::Offer { route, .. } => RouteControl::Offer {
                route,
                video: Vec::new(),
                audio: vec!["opus".into()],
                session: None,
                drive: None,
                room: None,
            },
            _ => unreachable!(),
        };
        let s = serde_json::to_string(&offered).unwrap();
        assert!(s.contains("\"audio\":[\"opus\"]"));
        let back: RouteControl = serde_json::from_str(&s).unwrap();
        assert_eq!(offered, back);
    }

    #[test]
    fn route_offer_session_accepts_skew_both_ways() {
        // An offer with no `session` — the v1 / new-shell case — decodes
        // as `None` (always a fresh shell), exactly today's behaviour.
        let legacy = r#"{"kind":"offer","route":{
            "id":"route:h:terminal→v:term-view:1","from":"h:terminal","to":"v:term-view:1","media":"generic"
        }}"#;
        let rc: RouteControl = serde_json::from_str(legacy).unwrap();
        assert!(matches!(rc, RouteControl::Offer { session: None, .. }));

        // `None` serializes *without* the key, so an older host sees exactly
        // the offer shape it always did (and `video`/`audio` stay invisible).
        let s = serde_json::to_string(&rc).unwrap();
        assert!(!s.contains("session"));

        // An attach offer — `session: Some(id)` — round-trips and rides the
        // wire under `"session"`.
        let attach = match rc {
            RouteControl::Offer { route, .. } => RouteControl::Offer {
                route,
                video: Vec::new(),
                audio: Vec::new(),
                session: Some("term-3".into()),
                drive: None,
                room: None,
            },
            _ => unreachable!(),
        };
        let s = serde_json::to_string(&attach).unwrap();
        assert!(s.contains("\"session\":\"term-3\""));
        let back: RouteControl = serde_json::from_str(&s).unwrap();
        assert_eq!(attach, back);
    }

    #[test]
    fn route_accept_session_accepts_skew_both_ways() {
        // An older host's accept has no `session` — it decodes as `None`
        // (the viewer just doesn't learn a shared id), never an error.
        let legacy = r#"{"kind":"accept","route_id":"r1"}"#;
        let rc: RouteControl = serde_json::from_str(legacy).unwrap();
        assert!(matches!(
            rc,
            RouteControl::Accept {
                session: None,
                paced_video: false,
                ..
            }
        ));
        let s = serde_json::to_string(&rc).unwrap();
        assert!(!s.contains("session"));
        assert!(!s.contains("paced_video"));

        // The terminal host echoes the resolved session id on accept.
        let accept = RouteControl::Accept {
            route_id: "r1".into(),
            session: Some("term-7".into()),
            paced_video: false,
        };
        let s = serde_json::to_string(&accept).unwrap();
        assert!(s.contains("\"session\":\"term-7\""));
        let back: RouteControl = serde_json::from_str(&s).unwrap();
        assert_eq!(accept, back);

        let paced = RouteControl::Accept {
            route_id: "screen".into(),
            session: None,
            paced_video: true,
        };
        let s = serde_json::to_string(&paced).unwrap();
        assert!(s.contains("\"paced_video\":true"));
        assert_eq!(serde_json::from_str::<RouteControl>(&s).unwrap(), paced);
    }

    #[test]
    fn terminal_sessions_round_trip_and_tag() {
        // The viewer's "list your sessions" request.
        let m = ControlMessage::Route(RouteControl::TerminalSessionsRequest);
        let j = serde_json::to_value(&m).unwrap();
        assert_eq!(j["t"], "route");
        assert_eq!(j["kind"], "terminal_sessions_request");
        assert_eq!(serde_json::from_value::<ControlMessage>(j).unwrap(), m);

        // The host's reply, carrying the open-session rows the picker shows.
        let m = ControlMessage::Route(RouteControl::TerminalSessions {
            sessions: vec![
                TerminalSessionInfo {
                    session_id: "term-1".into(),
                    title: "term-1".into(),
                    created_unix: 1_700_000_000,
                    attachers: 2,
                },
                TerminalSessionInfo {
                    session_id: "term-2".into(),
                    title: "vim".into(),
                    created_unix: 1_700_000_500,
                    attachers: 1,
                },
            ],
        });
        let j = serde_json::to_value(&m).unwrap();
        assert_eq!(j["kind"], "terminal_sessions");
        assert_eq!(j["sessions"][0]["session_id"], "term-1");
        assert_eq!(j["sessions"][0]["attachers"], 2);
        assert_eq!(j["sessions"][1]["title"], "vim");
        let back: ControlMessage = serde_json::from_value(j).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn old_peer_drops_terminal_session_kinds_to_unknown() {
        // An older build that never heard of the terminal-sessions kinds must
        // decode them as `Route(Unknown)` — never fail the control channel,
        // so the route/offer/accept traffic it *does* know keeps flowing.
        for v in [
            serde_json::json!({ "t": "route", "kind": "terminal_sessions_request" }),
            serde_json::json!({ "t": "route", "kind": "terminal_sessions", "sessions": [] }),
        ] {
            // Re-tag to an unknown kind to model the *old* peer's view: it
            // wouldn't recognise the kind name, so it lands on the catch-all.
            // (Here the build *does* know them, so we instead assert a truly
            // unknown nested kind still decodes — the same forward-compat
            // guarantee these ride behind.)
            let _known: ControlMessage = serde_json::from_value(v).expect("known kind decodes");
        }
        let unknown = serde_json::json!({ "t": "route", "kind": "terminal_attach_v2" });
        let m: ControlMessage =
            serde_json::from_value(unknown).expect("unknown kind must not error");
        assert_eq!(m, ControlMessage::Route(RouteControl::Unknown));
    }

    #[test]
    fn presence_without_ownership_fields_still_decodes() {
        // An older peer's advert has no owner/claimable — they default.
        let json = r#"{
            "protocol": 1, "node": "old", "label": "Old", "hostname": "old",
            "summary": {"os":"linux","cpu":"cpu","ram_bytes":1,"device_count":1}
        }"#;
        let p: NodeProfile = serde_json::from_str(json).unwrap();
        assert_eq!(p.owner, None);
        assert!(!p.claimable);
        assert_eq!(
            p.boot, 0,
            "an older peer reads as boot 0 (heartbeats instead)"
        );
    }

    #[test]
    fn owned_roster_round_trips() {
        let r = OwnedRoster {
            key: "a1b2c3".into(),
            name: "Casey".into(),
            version: 4,
            members: vec![
                OwnedMember {
                    device: "my-laptop".into(),
                    label: "My laptop".into(),
                    claimed_at: Some(1_700_000_000),
                },
                OwnedMember {
                    device: "spare-nuc".into(),
                    label: "Spare NUC".into(),
                    // Unstamped: a member whose record predates the field.
                    claimed_at: None,
                },
            ],
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: OwnedRoster = serde_json::from_str(&s).unwrap();
        assert_eq!(r, back);
    }

    #[test]
    fn owned_roster_tolerates_a_minimal_advert() {
        // A member from an older peer may carry just the device id — and no
        // fleet name (the field postdates them).
        let json = r#"{ "key": "k", "members": [{ "device": "d" }] }"#;
        let r: OwnedRoster = serde_json::from_str(json).unwrap();
        assert_eq!(r.version, 0);
        assert_eq!(r.members.len(), 1);
        assert_eq!(r.members[0].label, "");
        assert_eq!(r.name, "");

        // An unnamed fleet serializes *without* the key, so an older
        // receiver sees exactly the roster shape it always did.
        let s = serde_json::to_string(&r).unwrap();
        assert!(!s.contains("name"));
    }

    #[test]
    fn room_messages_round_trip_and_tag() {
        let m = RoomMessage {
            room: "room:abc".into(),
            name: "Movie night".into(),
            event: RoomEvent::Chat {
                text: "hi all".into(),
            },
        };
        let j = serde_json::to_value(&m).unwrap();
        assert_eq!(j["room"], "room:abc");
        assert_eq!(j["name"], "Movie night");
        assert_eq!(j["kind"], "chat");
        assert_eq!(j["text"], "hi all");
        let back: RoomMessage = serde_json::from_str(&j.to_string()).unwrap();
        assert_eq!(m, back);

        let m = RoomMessage {
            room: "room:abc".into(),
            name: "Movie night".into(),
            event: RoomEvent::Invite {
                members: vec!["a".into(), "b".into()],
                access: RoomAccess::Open,
            },
        };
        let j = serde_json::to_value(&m).unwrap();
        assert_eq!(j["access"], "open");
        let back: RoomMessage = serde_json::from_str(&j.to_string()).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn room_invite_without_access_reads_invite_only() {
        // An older host's invite carries no `access` — it must never read
        // as more open than the host meant.
        let json = r#"{ "room": "room:abc", "name": "Movie night",
                        "kind": "invite", "members": ["a"] }"#;
        let m: RoomMessage = serde_json::from_str(json).unwrap();
        assert_eq!(
            m.event,
            RoomEvent::Invite {
                members: vec!["a".into()],
                access: RoomAccess::Invite,
            }
        );
    }

    #[test]
    fn room_knock_and_deny_round_trip() {
        for (event, kind) in [(RoomEvent::Knock, "knock"), (RoomEvent::Deny, "deny")] {
            let m = RoomMessage {
                room: "room:owner:ab12cd34".into(),
                name: String::new(),
                event,
            };
            let j = serde_json::to_value(&m).unwrap();
            assert_eq!(j["kind"], kind);
            let back: RoomMessage = serde_json::from_str(&j.to_string()).unwrap();
            assert_eq!(m, back);
        }
    }

    #[test]
    fn room_merge_round_trips_the_canonical_room() {
        let m = RoomMessage {
            room: "room:b:duplicate".into(),
            name: "Alex + Casey".into(),
            event: RoomEvent::Merge {
                into: "room:a:canonical".into(),
                members: vec!["a".into(), "b".into()],
                access: RoomAccess::Invite,
            },
        };
        let j = serde_json::to_value(&m).unwrap();
        assert_eq!(j["kind"], "merge");
        assert_eq!(j["into"], "room:a:canonical");
        let back: RoomMessage = serde_json::from_value(j).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn room_shared_files_round_trip_and_tag() {
        // A member's offering to the host.
        let m = RoomMessage {
            room: "room:owner:ab12cd34".into(),
            name: String::new(),
            event: RoomEvent::ShareList {
                files: vec![SharedFileMeta {
                    token: "share_xyz".into(),
                    name: "deck.pdf".into(),
                    size: 4096,
                }],
            },
        };
        let j = serde_json::to_value(&m).unwrap();
        assert_eq!(j["kind"], "share_list");
        assert_eq!(j["files"][0]["token"], "share_xyz");
        let back: RoomMessage = serde_json::from_str(&j.to_string()).unwrap();
        assert_eq!(m, back);

        // The host's aggregated list, tagged with each uploader.
        let m = RoomMessage {
            room: "room:owner:ab12cd34".into(),
            name: "Movie night".into(),
            event: RoomEvent::Shares {
                files: vec![SharedEntry {
                    from: "alex".into(),
                    token: "share_xyz".into(),
                    name: "deck.pdf".into(),
                    size: 4096,
                }],
            },
        };
        let j = serde_json::to_value(&m).unwrap();
        assert_eq!(j["kind"], "shares");
        assert_eq!(j["files"][0]["from"], "alex");
        let back: RoomMessage = serde_json::from_str(&j.to_string()).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn room_close_round_trips() {
        let m = RoomMessage {
            room: "room:owner:ab12cd34".into(),
            name: "Movie night".into(),
            event: RoomEvent::Close,
        };
        let j = serde_json::to_value(&m).unwrap();
        assert_eq!(j["kind"], "close");
        let back: RoomMessage = serde_json::from_str(&j.to_string()).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn room_message_tolerates_a_minimal_advert() {
        // A join from a build that skipped the name still decodes.
        let json = r#"{ "room": "room:abc", "kind": "join" }"#;
        let m: RoomMessage = serde_json::from_str(json).unwrap();
        assert_eq!(m.room, "room:abc");
        assert_eq!(m.name, "");
        assert_eq!(m.event, RoomEvent::Join);
    }

    #[test]
    fn ownership_claim_round_trips_and_tags() {
        let m = ControlMessage::Ownership(OwnershipControl::Claim {
            owner: "my-laptop".into(),
        });
        let j = serde_json::to_value(&m).unwrap();
        assert_eq!(j["t"], "ownership");
        assert_eq!(j["kind"], "claim");
        assert_eq!(j["owner"], "my-laptop");
        let back: ControlMessage = serde_json::from_value(j).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn site_control_round_trips_and_tags() {
        // The "list your sites" request.
        let m = ControlMessage::Site(SiteControl::List);
        let j = serde_json::to_value(&m).unwrap();
        assert_eq!(j["t"], "site");
        assert_eq!(j["kind"], "list");
        assert_eq!(serde_json::from_value::<ControlMessage>(j).unwrap(), m);

        // The reply, carrying the full service list + the exposed map.
        let m = ControlMessage::Site(SiteControl::Sites {
            services: vec![SiteService {
                id: "tcp:3000".into(),
                name: "HTTP".into(),
                port: 3000,
                scheme: "http".into(),
                loopback: true,
                process: "grafana".into(),
                title: "My Grafana".into(),
            }],
            exposed: std::collections::BTreeMap::from([("tcp:3000".into(), "My Grafana".into())]),
        });
        let j = serde_json::to_value(&m).unwrap();
        assert_eq!(j["kind"], "sites");
        assert_eq!(j["services"][0]["port"], 3000);
        assert_eq!(j["exposed"]["tcp:3000"], "My Grafana");
        assert_eq!(serde_json::from_value::<ControlMessage>(j).unwrap(), m);

        // The "advertise exactly these" command.
        let m = ControlMessage::Site(SiteControl::SetExposed {
            exposed: std::collections::BTreeMap::from([("tcp:8080".into(), "App".into())]),
        });
        let j = serde_json::to_value(&m).unwrap();
        assert_eq!(j["kind"], "set_exposed");
        assert_eq!(serde_json::from_value::<ControlMessage>(j).unwrap(), m);
    }

    #[test]
    fn control_message_tags_are_stable() {
        let m = ControlMessage::Route(RouteControl::Reject {
            route_id: "r1".into(),
            reason: "not authorized".into(),
        });
        let j = serde_json::to_value(&m).unwrap();
        assert_eq!(j["t"], "route");
        assert_eq!(j["kind"], "reject");
        assert_eq!(j["reason"], "not authorized");
    }

    #[test]
    fn share_invite_carries_grants() {
        let m = ControlMessage::Share(ShareControl::Invite {
            from: Person {
                id: "person:me".into(),
                name: "Me".into(),
            },
            grants: vec![Grant {
                id: "g1".into(),
                media: MediaKind::Video,
                role: GrantRole::Consume,
                capability: None,
                label: "Receive your screen".into(),
            }],
        });
        let j = serde_json::to_value(&m).unwrap();
        assert_eq!(j["t"], "share");
        assert_eq!(j["kind"], "invite");
        let back: ControlMessage = serde_json::from_value(j).unwrap();
        assert_eq!(m, back);
    }

    // ---- forward-compatibility: a newer build's additions never break a
    // ---- consumer that doesn't recognise them ---------------------------

    #[test]
    fn control_message_unknown_top_tag_decodes_to_unknown() {
        // A `t` this build has never heard of — the shape of a control kind
        // a future release adds. It must decode (to `Unknown`), not error,
        // so the rest of the control channel keeps working.
        let v = serde_json::json!({ "t": "teleport", "whatever": 1 });
        let m: ControlMessage = serde_json::from_value(v).expect("unknown tag must not error");
        assert_eq!(m, ControlMessage::Unknown);
    }

    #[test]
    fn unknown_nested_route_kind_does_not_poison_control_message() {
        // The load-bearing case: a *new* RouteControl kind nested inside a
        // known `ControlMessage::Route`. An older peer must still decode the
        // envelope (as `Route(Unknown)`) instead of dropping the whole
        // message — that's what kept video handshakes brittle.
        let v = serde_json::json!({ "t": "route", "kind": "renegotiate", "route_id": "r1" });
        let m: ControlMessage =
            serde_json::from_value(v).expect("unknown nested kind must not error");
        assert_eq!(m, ControlMessage::Route(RouteControl::Unknown));
    }

    #[test]
    fn every_control_sub_enum_has_a_catch_all() {
        for v in [
            serde_json::json!({ "t": "share", "kind": "future" }),
            serde_json::json!({ "t": "ownership", "kind": "future" }),
            serde_json::json!({ "t": "site", "kind": "future" }),
            serde_json::json!({ "t": "app", "kind": "future" }),
        ] {
            serde_json::from_value::<ControlMessage>(v).expect("sub-enum must tolerate new kinds");
        }
    }

    #[test]
    fn room_message_unknown_event_decodes_via_flatten() {
        // RoomEvent rides RoomMessage `#[serde(flatten)]`, so this also
        // proves flatten + a catch-all variant cooperate: an unknown room
        // event keeps the roster/chat plane parseable.
        let v = serde_json::json!({ "room": "room:1", "name": "Den", "kind": "reaction" });
        let m: RoomMessage = serde_json::from_value(v).expect("unknown room event must not error");
        assert_eq!(m.event, RoomEvent::Unknown);
        assert_eq!(m.room, "room:1");
    }

    #[test]
    fn unknown_room_access_reads_as_invite_only() {
        // A future access policy must never stop an older member learning of
        // the room: it falls back to the safe (most restrictive) default.
        let v = serde_json::json!({
            "kind": "invite", "members": [], "access": "knock_with_password"
        });
        let m: RoomEvent = serde_json::from_value(v).expect("unknown access must not error");
        assert!(matches!(
            m,
            RoomEvent::Invite {
                access: RoomAccess::Invite,
                ..
            }
        ));
    }

    #[test]
    fn node_profile_ignores_unknown_fields() {
        // A profile from a newer build carries fields this one has never
        // seen; they must be ignored, not rejected (no `deny_unknown_fields`
        // may ever creep onto presence, or peers vanish from the graph).
        let json = r#"{
            "protocol": 1, "node": "new", "label": "New", "hostname": "new",
            "summary": {"os":"linux","cpu":"cpu","ram_bytes":1,"device_count":1},
            "teleport_range": 42, "vibes": ["immaculate"]
        }"#;
        serde_json::from_str::<NodeProfile>(json).expect("unknown profile fields must be ignored");
    }

    #[test]
    fn node_profile_decodes_with_missing_or_partial_fields() {
        // The mirror of the unknown-fields rule: a *missing* or partial field
        // must not drop the whole advert either. A peer mid-scan (no summary
        // yet), or one that omits `protocol`/`label`, still decodes — only those
        // fields fall back to their defaults. This is the fix for the "we get
        // presence updates but silently discard them in the parse" bug, where a
        // single absent required field failed the entire `NodeProfile` decode.
        let json = r#"{ "node": "scanning" }"#;
        let p: NodeProfile =
            serde_json::from_str(json).expect("a sparse advert must still decode, not drop");
        assert_eq!(p.node.as_str(), "scanning");
        assert_eq!(p.protocol, 0);
        assert!(p.label.is_empty());
        assert_eq!(p.summary, InventorySummary::default());

        // A partial summary (missing some hardware fields) also decodes — the
        // absent fields default rather than failing the whole profile.
        let json = r#"{ "node": "n", "summary": { "os": "linux" } }"#;
        let p: NodeProfile =
            serde_json::from_str(json).expect("a partial summary must not drop the profile");
        assert_eq!(p.summary.os, "linux");
        assert_eq!(p.summary.cpu, "");
        assert_eq!(p.summary.ram_bytes, 0);
    }
}
