//! The CEC service data model — every value that crosses the HTTP boundary
//! between AllMyStuff and the hosted Critical Error Computing backend.
//!
//! These types **are** the API contract (see `CONTRACT.md`). They serialise
//! as `snake_case` JSON, every enum is tagged so the wire stays
//! self-describing, and every response struct tolerates extra fields so the
//! backend can grow without breaking an older app — the same forward-compat
//! discipline `allmystuff-protocol` keeps for the mesh wire.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Account & entitlements
// ---------------------------------------------------------------------------

/// What an account is allowed to do. An account is *optional* — the free app
/// never needs one — and a single account can be a customer, an agent, or
/// both (the same human can be a household's IT person and a CEC technician).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountRole {
    /// A customer: presses Ask-for-Help, rents a Private Line, owns the CEC
    /// mesh for their devices.
    Customer,
    /// A CEC technician: signs in, goes online, handles help sessions.
    Agent,
}

/// A signed-in account. Identity proof is the email one-time code; the
/// `device_ids` are the mesh pubkeys the human has bound, so the backend
/// knows which nodes to provision the CEC mesh for and pre-trust.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    /// Opaque, stable account id. The per-customer CEC network id is derived
    /// from this (see [`crate::convention`]).
    pub id: String,
    pub email: String,
    /// Display name shown to the other side of a help session ("Casey", or a
    /// technician's "Sam @ CEC"). Defaults to the email's local part.
    #[serde(default)]
    pub display_name: String,
    /// The hats this account wears. Always at least `Customer`.
    #[serde(default)]
    pub roles: Vec<AccountRole>,
    /// Mesh device pubkeys bound to this account (bare `public_id`, never the
    /// `pubkey-SUFFIX` display form).
    #[serde(default)]
    pub device_ids: Vec<String>,
}

impl Account {
    pub fn is_agent(&self) -> bool {
        self.roles.contains(&AccountRole::Agent)
    }
    pub fn is_customer(&self) -> bool {
        self.roles.contains(&AccountRole::Customer)
    }
}

/// The three Concierge tiers exactly as advertised on the site. The display
/// strings and prices live with the tier so the app and the reference server
/// agree on what each one promises.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConciergeTier {
    /// "Pay as you go" — $25 / 15 min, no monthly.
    PayAsYouGo,
    /// "Priority" — $19 / mo, 30 min included, priority queue.
    Priority,
    /// "Looked after" — $69 / mo, 90 min included, scheduled check-ins.
    LookedAfter,
}

impl ConciergeTier {
    /// The marketing label, verbatim from allmystuff.works/service.
    pub fn label(self) -> &'static str {
        match self {
            ConciergeTier::PayAsYouGo => "Pay as you go",
            ConciergeTier::Priority => "Priority",
            ConciergeTier::LookedAfter => "Looked after",
        }
    }
    /// The product code on the site (SV-01 … SV-03).
    pub fn product_code(self) -> &'static str {
        match self {
            ConciergeTier::PayAsYouGo => "SV-01",
            ConciergeTier::Priority => "SV-02",
            ConciergeTier::LookedAfter => "SV-03",
        }
    }
}

/// What a customer's account currently entitles them to. The app reads this
/// to decide which buttons to show: Ask-for-Help needs `concierge.is_some()`,
/// and the CEC mesh is provisioned when `hardware || private_line ||
/// concierge.is_some()`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Entitlements {
    /// They own CEC hardware (an Access box) registered to this account.
    #[serde(default)]
    pub hardware: bool,
    /// They have at least one active Private Line subscription.
    #[serde(default)]
    pub private_line: bool,
    /// Their active Concierge tier, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concierge: Option<ConciergeTier>,
}

impl Entitlements {
    /// Whether anything here warrants the dedicated, isolated CEC mesh for
    /// this customer — "CEC hardware OR services" in product terms.
    pub fn wants_cec_mesh(&self) -> bool {
        self.hardware || self.private_line || self.concierge.is_some()
    }
    /// Whether the Ask-for-Help button should be live.
    pub fn can_ask_for_help(&self) -> bool {
        self.concierge.is_some()
    }
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

/// `POST /v1/auth/start` — request a one-time sign-in code by email.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartSignIn {
    pub email: String,
}

/// The backend's acknowledgement that a code was sent (it never reveals the
/// code, naturally — except the mock, which prints it locally).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartSignInResponse {
    #[serde(default = "default_true")]
    pub sent: bool,
    /// e.g. "c•••@gmail.com" for the UI to echo back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub masked_email: Option<String>,
}

fn default_true() -> bool {
    true
}

/// `POST /v1/auth/verify` — exchange the email + code for a session, binding
/// this device's mesh identity in the same call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifySignIn {
    pub email: String,
    pub code: String,
    /// This device's bare mesh pubkey, bound to the account so the backend can
    /// provision its CEC mesh. Optional: a headless agent may bind later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    /// A friendly label for the bound device ("Casey's laptop").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_label: Option<String>,
}

/// A live session: the bearer token plus a snapshot of the account so the app
/// can render immediately without a second round trip.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub token: String,
    pub account: Account,
    #[serde(default)]
    pub entitlements: Entitlements,
}

/// `GET /v1/me` — the current account + entitlements for a bearer token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Me {
    pub account: Account,
    #[serde(default)]
    pub entitlements: Entitlements,
}

/// `POST /v1/me/device` — bind (or re-label) a mesh device on the account.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BindDevice {
    pub device_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

// ---------------------------------------------------------------------------
// Venues (Private Line + the CEC mesh share this shape)
// ---------------------------------------------------------------------------

/// A TURN relay credential, matching the app's `TurnEntry` / network-config
/// `turn_servers` shape so a venue drops straight into a `NetworkConfig`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnCredential {
    pub url: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub credential: String,
}

/// The three servers that make up a "venue" — signaling, STUN, TURN. The
/// backend can either hand the resolved servers inline *and* point at a live
/// venue file via `url` (so the app tracks the host's updates the way a
/// remote venue does in `venue-settings.ts`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct VenueSpec {
    /// A live venue-file URL the app can re-fetch (`GET` → an
    /// `allmystuff.venue` envelope). Present for CEC-hosted venues.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default)]
    pub signaling: Vec<String>,
    #[serde(default)]
    pub stun: Vec<String>,
    #[serde(default)]
    pub turn: Vec<TurnCredential>,
}

/// The `allmystuff.venue` file envelope served at a [`VenueSpec::url`]. This
/// is byte-compatible with the app's `venue-settings.ts::fetchVenueServers`,
/// so a CEC venue is just a remote venue the app already knows how to load.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VenueFile {
    pub kind: String,
    pub version: u32,
    pub label: String,
    #[serde(default)]
    pub signaling_servers: Vec<String>,
    #[serde(default)]
    pub stun_servers: Vec<String>,
    #[serde(default)]
    pub turn_servers: Vec<TurnCredential>,
}

impl VenueFile {
    pub const KIND: &'static str = "allmystuff.venue";
    pub const VERSION: u32 = 1;

    pub fn new(label: impl Into<String>, spec: &VenueSpec) -> Self {
        VenueFile {
            kind: Self::KIND.into(),
            version: Self::VERSION,
            label: label.into(),
            signaling_servers: spec.signaling.clone(),
            stun_servers: spec.stun.clone(),
            turn_servers: spec.turn.clone(),
        }
    }
}

/// One Private Line subscription — "a venue of your own", $10/mo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrivateLine {
    pub id: String,
    pub label: String,
    pub status: SubscriptionStatus,
    /// The servers (and live venue-file URL) this Private Line provides.
    pub venue: VenueSpec,
    #[serde(default)]
    pub monthly_price_cents: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionStatus {
    Active,
    Cancelled,
    PastDue,
}

/// `POST /v1/private-line` — rent a new Private Line.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RentPrivateLine {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

// ---------------------------------------------------------------------------
// The CEC mesh
// ---------------------------------------------------------------------------

/// `POST /v1/mesh/provision` — the descriptor the app needs to stand up the
/// customer's isolated `cec-customer-<hash>` network: which network id to
/// join, which venue serves it, and the single **CEC Service** node id to
/// pre-trust. Every CEC connection to this customer rides that one node;
/// individual agents live *behind* the backend, never as mesh peers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshProvision {
    /// The normalised network id (`cec-customer-<hash>`).
    pub network_id: String,
    /// What to label the network in the app ("CEC").
    pub label: String,
    /// The CEC-hosted venue (its own signaling/STUN/TURN), serving only this
    /// customer's devices and the CEC Service node.
    pub venue: VenueSpec,
    /// The bare pubkey of the single CEC Service node. Pre-approve this in the
    /// network's roster; it is the only non-customer peer that ever appears.
    pub cec_service_node_id: String,
    /// Always true for a CEC mesh — the customer auto-approves CEC's node and
    /// their own fleet without a verification dance.
    #[serde(default = "default_true")]
    pub auto_approve: bool,
}

// ---------------------------------------------------------------------------
// Ask-for-Help (Concierge) — customer side
// ---------------------------------------------------------------------------

/// `POST /v1/help` — a customer presses Ask-for-Help. The app has already
/// minted a host-side help room on the CEC network; it hands the room id over
/// so the dispatched agent can join it as the CEC Service node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskForHelp {
    /// The CEC network the room lives on (`cec-customer-<hash>`).
    pub network_id: String,
    /// The customer-hosted room id (`room:{host}:cec-{nonce}`).
    pub room_id: String,
    /// The device that's asking (bare pubkey), so the agent knows where to
    /// land.
    pub device_id: String,
    /// One line of "what's wrong", optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HelpStatus {
    /// Waiting for an online agent to pick it up.
    Queued,
    /// An agent accepted; they're connecting as the CEC Service node.
    Assigned,
    /// The agent is in the room.
    Connected,
    /// The session is over.
    Ended,
    /// The customer cancelled before an agent arrived.
    Cancelled,
}

impl HelpStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, HelpStatus::Ended | HelpStatus::Cancelled)
    }
}

/// A help session, as seen by both the customer (polling `GET /v1/help/{id}`)
/// and the agent queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelpSession {
    pub id: String,
    pub status: HelpStatus,
    pub network_id: String,
    pub room_id: String,
    pub cec_service_node_id: String,
    /// The customer device that asked (bare pubkey).
    pub customer_device_id: String,
    /// A label for the customer the agent sees ("Casey").
    #[serde(default)]
    pub customer_label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    /// The handling technician's display name, once assigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_label: Option<String>,
    /// Unix seconds.
    #[serde(default)]
    pub created_at: u64,
}

// ---------------------------------------------------------------------------
// Agent side
// ---------------------------------------------------------------------------

/// `POST /v1/agent/presence` — a technician toggles "online and available to
/// handle requests".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetPresence {
    pub online: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentPresence {
    pub online: bool,
    /// Unix seconds the agent has been online since (when online).
    #[serde(default)]
    pub since: u64,
}

/// What an agent receives when they accept a help session: everything needed
/// to join the customer's CEC network as the CEC Service node. The agent's
/// link to the customer is brokered by the backend through that one node —
/// the agent is not, itself, a mesh peer the customer sees.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentAssignment {
    pub session: HelpSession,
    /// The venue to connect through (the CEC-hosted servers for this
    /// customer).
    pub venue: VenueSpec,
}

// ---------------------------------------------------------------------------
// Errors on the wire
// ---------------------------------------------------------------------------

/// The body shape of a non-2xx response. Both fields optional so a bare
/// `{ "error": "..." }` or a richer `{ "error": { "code", "message" } }` both
/// parse.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ApiErrorBody {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

// ---------------------------------------------------------------------------
// Mock-only / dev helpers (the reference server implements these; a real
// backend would not). Kept in the contract so tests and tooling share them.
// ---------------------------------------------------------------------------

/// `POST /v1/dev/grant` — MOCK ONLY. Set up an account's entitlements / agent
/// role for tests and local demos.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DevGrant {
    pub email: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entitlements: Option<Entitlements>,
    /// Make this account an agent.
    #[serde(default)]
    pub agent: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enums_are_snake_case_tagged() {
        assert_eq!(
            serde_json::to_value(ConciergeTier::PayAsYouGo).unwrap(),
            serde_json::json!("pay_as_you_go")
        );
        assert_eq!(
            serde_json::to_value(HelpStatus::Queued).unwrap(),
            serde_json::json!("queued")
        );
        assert_eq!(
            serde_json::to_value(AccountRole::Agent).unwrap(),
            serde_json::json!("agent")
        );
    }

    #[test]
    fn account_tolerates_missing_optional_fields() {
        let a: Account = serde_json::from_str(r#"{"id":"a1","email":"x@y.z"}"#).unwrap();
        assert_eq!(a.id, "a1");
        assert!(a.roles.is_empty());
        assert!(a.device_ids.is_empty());
    }

    #[test]
    fn entitlements_drive_the_two_decisions() {
        let none = Entitlements::default();
        assert!(!none.wants_cec_mesh());
        assert!(!none.can_ask_for_help());

        let hw = Entitlements {
            hardware: true,
            ..Default::default()
        };
        assert!(hw.wants_cec_mesh());
        assert!(!hw.can_ask_for_help());

        let concierge = Entitlements {
            concierge: Some(ConciergeTier::Priority),
            ..Default::default()
        };
        assert!(concierge.wants_cec_mesh());
        assert!(concierge.can_ask_for_help());
    }

    #[test]
    fn error_body_parses_both_shapes() {
        let rich: ApiErrorBody =
            serde_json::from_str(r#"{"code":"bad_code","message":"nope"}"#).unwrap();
        assert_eq!(rich.code.as_deref(), Some("bad_code"));
        let bare: ApiErrorBody = serde_json::from_str(r#"{}"#).unwrap();
        assert!(bare.message.is_none());
    }
}
