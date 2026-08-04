//! # allmystuff-cec-protocol
//!
//! The wire contract and shared constants for **CEC Support** — Critical
//! Error Computing's one-tap remote help desk. It lives in the AllMyStuff
//! workspace because both sides speak it: the technician side (an AllMyStuff
//! install joined to a customer's support mesh) and the standalone CEC Support
//! client app, which reuses this crate through the shared node engine.
//!
//! CEC Support rides on the [MyOwnMesh](https://github.com/mrjeeves/MyOwnMesh)
//! peer-to-peer substrate, but with two deliberate twists that make it behave
//! like AnyDesk rather than an ordinary always-on mesh:
//!
//! 1. **Signaling-only presence, Silent areas.** Every CEC node — the customer
//!    running the CEC Support app, and every technician's AllMyStuff install —
//!    lives on the one well-known **Silent** mesh, [`HELP_NETWORK_ID`]
//!    (`cecsupport-clients`). MyOwnMesh is a mesh *signaling* system for
//!    direct WebRTC peer-to-peer connections, and a Silent network uses only
//!    that half: members are visible to each other at the signaling layer
//!    (announces in the room) but **nothing ever connects on its own** — no
//!    auto-dial, no roster gossip, no relaying of data or media through
//!    anything (the sole data-path fallback anywhere is a TURN server when
//!    NAT rules out a direct path, which is WebRTC's own machinery, not a
//!    mesh hop). Raising a hand is **joining a second Silent room**,
//!    [`ASK_NETWORK_ID`] (`cecsupport-asking`): presence in that room *is*
//!    the queue entry, and lowering the hand is leaving it. The customer's
//!    short [`SupportId`](ids::SupportId) number is a display/verification
//!    label derived from the device key (readable over the phone as the
//!    fallback when the queue is crowded), never a room name. A mesh is just
//!    a signaling namespace — the standing area carries discoverability and
//!    the session; the asking room carries exactly one bit, "help".
//! 2. **Deliberate dial, then approve.** The technician answers a raised
//!    hand (or resolves a phoned-in number to its device) and *explicitly*
//!    dials that device on the area. The customer then approves with one of
//!    three choices — [`ApprovalScope`] `Once` / `ThreeHours` / `Forever` —
//!    and can revoke at any time. Approval state lives in
//!    `allmystuff-cec-consent`; this crate defines only the *messages* and
//!    the *scope*.
//!
//! ## Isolation from other MyOwnMesh ecosystems
//!
//! CEC Support forks MyOwnMesh's signing tags and home dir so its signatures
//! never cross-verify against an AllMyStuff / MyOwnMesh / MyOwnLLM mesh and its
//! identity + state never collide with an existing install. It deliberately does
//! *not* fork the signaling app-id: the support area's well-known `network_id`
//! seeds a distinct room handle, so technician and customer meet on the
//! default app-id with no env override.
//!
//! - [`CEC_SIGN_DOMAIN_TAG`] / [`CEC_SIGN_DOMAIN_TAG_STATE`] — domain-separated
//!   signing tags.
//! - [`CEC_HOME_ENV`] — a CEC-specific home dir (`MYOWNMESH_HOME` override) so
//!   identity + state never collide with an existing AllMyStuff install.

pub mod ids;
pub mod media;
pub mod wire;

pub use ids::{
    device_pubkey, format_support_id, support_id_from_device, support_id_from_string, SupportId,
    SUPPORT_ID_LEN,
};
pub use media::{
    decode_media_frame, encode_media_frame, MediaFrame, MEDIA_KIND_AUDIO, MEDIA_KIND_VIDEO,
};
pub use wire::{
    AppControl, ApprovalScope, ChatMessage, ConnectControl, ControlMessage, ElevationBlocker,
    ElevationReport, Role, SupportPresence,
};

/// Prefix the retired per-number rooms carried (`cec-<9 digits>`). Kept
/// solely so upgrading nodes can recognise and purge the legacy rooms older
/// builds persisted — nothing derives new ids from it. (The NanoKVM claim
/// meshes also start with `cec-` but never match the digits-only tail.)
pub const CEC_NETWORK_PREFIX: &str = "cec-";

/// The one well-known **standing support area** every CEC node lives on, from
/// bring-up. A **Silent** mesh: residents are discoverable at the signaling
/// layer (that's how a technician's pinned redial finds a rebooted customer,
/// and how a phoned-in number resolves to a device) but nobody connects to
/// anybody until a technician deliberately dials one device. Sessions ride
/// this area as direct WebRTC connections; the consent handshake still gates
/// everything, so the area carries *presence*, never access.
pub const HELP_NETWORK_ID: &str = "cecsupport-clients";

/// The well-known **asking room** — the help queue itself. Also Silent, and
/// joined **only while this device's hand is up**: membership in this room is
/// the entire "I need help" signal (technicians watching the queue read the
/// room's signaling presence; the dialable number derives from each present
/// device id), and leaving the room withdraws the ask. Nothing connects here,
/// ever — a technician answers by dialing the device on [`HELP_NETWORK_ID`].
/// Replaces the `cec.presence` channel beacons, which needed data-channel
/// wires that a Silent area rightly never opens.
pub const ASK_NETWORK_ID: &str = "cecsupport-asking";

/// Domain-separation tag for the per-peer ed25519 auth handshake. Forked from
/// `myownmesh-mesh-auth-v1:` so a signature obtained on a CEC mesh cannot be
/// replayed on any other MyOwnMesh network, and vice-versa.
pub const CEC_SIGN_DOMAIN_TAG: &str = "cec-support-mesh-auth-v1:";

/// Domain-separation tag for signed network-state transitions.
pub const CEC_SIGN_DOMAIN_TAG_STATE: &str = "cec-support-network-state-v1:";

/// Env var naming the CEC Support home dir (a `MYOWNMESH_HOME` override), so
/// identity + rosters live under their own tree and never collide with an
/// existing AllMyStuff / MyOwnMesh install on the same machine.
pub const CEC_HOME_ENV: &str = "CEC_SUPPORT_HOME";

/// Wire-protocol version. Stays at 1 across additive changes; every message
/// enum carries an `Unknown` catch-all so an older peer never fails a decode.
pub const PROTOCOL_VERSION: u32 = 1;

/// Typed-channel name carrying [`SupportPresence`] beacons.
pub const CHANNEL_PRESENCE: &str = "cec.presence";
/// Typed-channel name carrying point-to-point [`ControlMessage`]s
/// (connect-request / approve / deny / end, and app control).
pub const CHANNEL_CONTROL: &str = "cec.control";
/// Channel name carrying the binary media plane (screen frames / input).
pub const CHANNEL_MEDIA: &str = "cec.media";

/// Capability tag advertised by every CEC Support node so peers can tell CEC
/// traffic from anything else sharing the substrate.
pub const CAP_TAG: &str = "cec-support";
/// Capability tag identifying a customer (help-seeker) node.
pub const ROLE_CLIENT_TAG: &str = "cec-client";
/// Capability tag identifying a technician (help-desk) node.
pub const ROLE_TECH_TAG: &str = "cec-tech";

/// Seconds in the "Auto-Approve for 3 hours" window.
pub const THREE_HOURS_SECS: u64 = 3 * 60 * 60;

/// This build's version string (`CARGO_PKG_VERSION`).
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
