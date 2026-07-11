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
//! 1. **One shared support area, hub-shaped.** Every CEC node — the customer
//!    running the CEC Support app, and every technician's AllMyStuff install —
//!    lives on the one well-known mesh, [`HELP_NETWORK_ID`]
//!    (`cecsupport-clients`). Under its hub topology (`CEC_HELP_HUBS`),
//!    customers hold connections only to CEC-operated infra hubs — never to
//!    each other — and see nobody. Raising a hand is a
//!    [`SupportPresence`] beacon on the area; the customer's short
//!    [`SupportId`](ids::SupportId) number is a display/verification label
//!    derived from the device key (readable over the phone as the fallback
//!    when the queue is crowded), never a room name. A mesh is just a
//!    signaling namespace — one area carries discovery, hand-raising, and
//!    the session itself.
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
    AppControl, ApprovalScope, ConnectControl, ControlMessage, PurchaseControl, PurchaseState,
    Role, SupportPresence,
};

/// Prefix the retired per-number rooms carried (`cec-<9 digits>`). Kept
/// solely so upgrading nodes can recognise and purge the legacy rooms older
/// builds persisted — nothing derives new ids from it. (The NanoKVM claim
/// meshes also start with `cec-` but never match the digits-only tail.)
pub const CEC_NETWORK_PREFIX: &str = "cec-";

/// The one well-known **global help mesh** every CEC client shares. A customer
/// who taps "Ask for help" joins it and beacons a [`SupportPresence`] there —
/// the beacon (its `support_id` is their dialable number) is the whole signal.
/// Technicians sit on the room and list the beacons; an actual session still
/// goes through the customer's own number mesh and the consent handshake, so
/// this room carries *want*, never access.
pub const HELP_NETWORK_ID: &str = "cecsupport-clients";

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

/// The one place a customer is ever sent to pay: the CEC-owned purchase page,
/// which hands off to the store's hosted checkout. The customer's app builds
/// this URL **itself** (appending its own support number / reference — see
/// [`PurchaseControl`](wire::PurchaseControl)); it is deliberately never taken
/// from the wire, so no peer can steer a customer's browser anywhere else.
/// Store domain / product config live behind this page, so they can change
/// without shipping a new app.
pub const DIAGNOSTIC_BUY_URL: &str = "https://support.cec.direct/buy/diagnostic/";

/// Display fallbacks for the diagnostic-session ask, used when a
/// [`PurchaseControl::Request`](wire::PurchaseControl::Request) arrives with
/// empty display fields. The checkout page stays authoritative for the charge.
pub const DIAGNOSTIC_ITEM: &str = "CEC Diagnostic Session";
/// See [`DIAGNOSTIC_ITEM`].
pub const DIAGNOSTIC_PRICE: &str = "$50";

/// This build's version string (`CARGO_PKG_VERSION`).
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
