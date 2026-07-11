//! Peer-to-peer message types for CEC Support.
//!
//! Two things ride the mesh: a [`SupportPresence`] beacon (broadcast, so a
//! technician can find a customer), and point-to-point [`ControlMessage`]s
//! (the connect-request → approve/deny → end handshake, app control, and the
//! mid-session purchase-request handshake).
//!
//! Every enum is internally-tagged serde with an `Unknown` catch-all and every
//! additive field is `#[serde(default)]`, so a newer peer's extra variant or
//! field never fails an older peer's decode — the same forward/backward-skew
//! discipline the AllMyStuff protocol uses.

use serde::{Deserialize, Serialize};

use crate::PROTOCOL_VERSION;

/// Which side of a CEC Support session a node is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// A customer seeking help. This is what the CEC Support app runs as.
    #[default]
    Client,
    /// A CEC technician (an AllMyStuff install joined to the secret mesh).
    Technician,
}

/// A presence beacon a node broadcasts on [`CHANNEL_PRESENCE`](crate::CHANNEL_PRESENCE).
///
/// A technician's app collects these (plus signaling-level sightings) to build
/// the pool of reachable customers; matching a typed Support ID against
/// `support_id` (or against `support_id_from_device(device_id)`) is how "dial
/// by number" resolves to a peer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupportPresence {
    /// Wire-protocol version of the sender.
    #[serde(default = "default_protocol")]
    pub protocol: u32,
    /// The sender's canonical device id (base32 ed25519 pubkey).
    pub device_id: String,
    /// The sender's Support ID (derivable from `device_id`; included so a UI
    /// can show it without recomputing).
    #[serde(default)]
    pub support_id: String,
    /// Friendly label. For a customer, a machine name; for a technician, the
    /// **Agent Name** shown in "*so-and-so* is trying to connect".
    #[serde(default)]
    pub label: String,
    /// Client or technician.
    #[serde(default)]
    pub role: Role,
    /// Whether this node is currently accepting connections. A customer sets
    /// this false to disappear ("stop sharing") without leaving the mesh.
    #[serde(default = "default_true")]
    pub available: bool,
    /// The sender's app version.
    #[serde(default)]
    pub app_version: String,
    /// OS string, e.g. `"windows"`, for the technician's node card.
    #[serde(default)]
    pub os: String,
    /// Hostname, best-effort.
    #[serde(default)]
    pub hostname: String,
    /// Per-run boot id — changes each launch so a restart is detectable.
    #[serde(default)]
    pub boot: u64,
    /// Unix seconds the beacon was sent.
    #[serde(default)]
    pub sent_at: u64,
}

impl SupportPresence {
    /// Build a beacon for `device_id`, filling `support_id` from it and
    /// defaulting the rest. Callers set `label`/`os`/`hostname`/etc.
    pub fn new(device_id: impl Into<String>, role: Role) -> Self {
        let device_id = device_id.into();
        let support_id = crate::ids::support_id_from_device(&device_id);
        SupportPresence {
            protocol: PROTOCOL_VERSION,
            device_id,
            support_id,
            label: String::new(),
            role,
            available: true,
            app_version: crate::APP_VERSION.to_string(),
            os: std::env::consts::OS.to_string(),
            hostname: String::new(),
            boot: 0,
            sent_at: 0,
        }
    }
}

/// How long an approval lasts — the customer's three choices in the
/// "*so-and-so* is trying to connect" prompt.
///
/// This governs *persistence and expiry*, which the `cec-support-consent`
/// store enforces:
///
/// | Variant      | UI label                    | Persists across restart? | Expires? |
/// |--------------|-----------------------------|--------------------------|----------|
/// | [`Once`]     | Approve Once                | no (in-memory only)      | at session end |
/// | [`ThreeHours`]| Auto-Approve for 3 hours   | yes                      | after 3 hours |
/// | [`Forever`]  | Auto-Approve Forever        | yes                      | never (until revoked) |
///
/// [`Once`]: ApprovalScope::Once
/// [`ThreeHours`]: ApprovalScope::ThreeHours
/// [`Forever`]: ApprovalScope::Forever
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ApprovalScope {
    /// This session only. Never written to disk; gone on restart or session end.
    Once,
    /// Auto-approve for the next 3 hours, then prompt again. Persisted with an
    /// expiry so it survives a reboot mid-repair.
    ThreeHours,
    /// Auto-approve until the customer revokes ("Forget this technician").
    Forever,
}

impl ApprovalScope {
    /// Whether a grant with this scope should be written to disk (survive a
    /// restart). `Once` is in-memory only.
    pub fn persists(self) -> bool {
        !matches!(self, ApprovalScope::Once)
    }

    /// The absolute expiry (unix seconds) for a grant made at `granted_at`,
    /// or `None` for scopes that don't time out.
    pub fn expires_at(self, granted_at: u64) -> Option<u64> {
        match self {
            ApprovalScope::ThreeHours => Some(granted_at.saturating_add(crate::THREE_HOURS_SECS)),
            ApprovalScope::Once | ApprovalScope::Forever => None,
        }
    }

    /// The customer-facing label.
    pub fn label(self) -> &'static str {
        match self {
            ApprovalScope::Once => "Approve Once",
            ApprovalScope::ThreeHours => "Auto-Approve for 3 hours",
            ApprovalScope::Forever => "Auto-Approve Forever",
        }
    }
}

/// The connect handshake, carried inside [`ControlMessage::Connect`] on
/// [`CHANNEL_CONTROL`](crate::CHANNEL_CONTROL).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ConnectControl {
    /// Technician → customer: "I'd like to connect." `agent_name` is what the
    /// customer sees ("*Agent Name* is trying to connect to your computer");
    /// `want_control` distinguishes view-only from full keyboard/mouse control.
    Request {
        session_id: String,
        #[serde(default)]
        agent_name: String,
        #[serde(default)]
        want_control: bool,
    },
    /// Customer → technician: approved, with the chosen [`ApprovalScope`].
    Approve {
        session_id: String,
        scope: ApprovalScope,
    },
    /// Customer → technician: declined.
    Deny {
        session_id: String,
        #[serde(default)]
        reason: String,
    },
    /// Either side ends the session (customer hang-up, revoke, or technician
    /// disconnect).
    End { session_id: String },
    /// Forward-compat: an unrecognised kind decodes here and is ignored.
    #[serde(other)]
    Unknown,
}

/// App-level actions a technician asks a customer's node to run on itself,
/// carried inside [`ControlMessage::App`]. Honoured only while an approval
/// covers the requesting technician.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum AppControl {
    /// Install CEC Support as a background service so it reconnects after
    /// reboots (the AnyDesk "unattended access" action). The customer's node
    /// still gates every future session on their standing approval.
    InstallService,
    /// Remove the background service.
    UninstallService,
    /// Restart the CEC Support agent.
    Restart,
    /// Forward-compat catch-all.
    #[serde(other)]
    Unknown,
}

/// Where a requested purchase stands, as reported by the customer's app in
/// [`PurchaseControl::Status`]. Display-state only — the *authoritative* record
/// of payment is the store order the technician verifies out-of-band before
/// sending [`PurchaseControl::Confirm`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum PurchaseState {
    /// The customer's app displayed the request (also tells the technician the
    /// customer's app is new enough to know about purchases at all).
    Seen,
    /// The customer opened the secure checkout in their browser.
    Opened,
    /// The customer says they completed the purchase. A *claim*, not proof —
    /// the technician verifies the order in the store before confirming.
    Claimed,
    /// The customer declined to purchase.
    Declined,
    /// Forward-compat catch-all.
    #[serde(other)]
    Unknown,
}

/// The purchase handshake, carried inside [`ControlMessage::Purchase`] on
/// [`CHANNEL_CONTROL`](crate::CHANNEL_CONTROL).
///
/// This is how a technician asks the customer to complete a purchase (the $50
/// diagnostic session) — before answering their help call, mid-session, or
/// after disconnecting to quote the work. Deliberate properties:
///
/// - **Technician-triggered only.** The customer's app never initiates one; it
///   only ever answers a [`Request`](PurchaseControl::Request).
/// - **Same trust bar as the connect prompt.** A Request needs no prior grant
///   — reaching the customer's number-derived room is the discovery gate, the
///   prompt names the asker (`agent_name`, exactly like
///   [`ConnectControl::Request`]), and the customer verifies the name against
///   the technician on the phone and can always decline. When a live grant
///   exists, the customer's node prefers the *grant's* name over the wire's.
/// - **No money moves on the mesh.** The wire carries only *display strings*
///   and state. Payment happens in the customer's own browser on the store's
///   hosted checkout; the checkout URL is a constant built into the customer's
///   app ([`DIAGNOSTIC_BUY_URL`](crate::DIAGNOSTIC_BUY_URL)) — never taken from
///   the wire, so a peer can't steer the customer's browser anywhere else.
/// - **Human confirmation closes the loop.** The technician sees the order
///   arrive in the store (tagged with the customer's support number), then
///   sends [`Confirm`](PurchaseControl::Confirm). No webhooks, no server —
///   the same person-checks-person shape as the agent-name verification.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum PurchaseControl {
    /// Technician → customer: "please complete this purchase before we
    /// continue." `item`/`price`/`note` are display strings for the customer's
    /// prompt (the checkout page stays authoritative for the real charge); a
    /// customer app with no defaults of its own may show them verbatim.
    ///
    /// Re-sent on a short loop until a [`Status`](PurchaseControl::Status)
    /// answers (the same delivery discipline as the connect handshake); the
    /// customer's node dedupes by `purchase_id`.
    Request {
        /// Unique id for this ask, minted by the technician's node — lets a
        /// withdrawn or re-issued request be told apart from the original.
        purchase_id: String,
        /// The CEC session this ask belongs to — empty for an ask made outside
        /// one (before answering a help call, or after disconnecting).
        session_id: String,
        /// The asker's Agent Name, for the prompt — same trust as the connect
        /// prompt's name: the customer checks it against the person on the
        /// phone. Ignored in favour of the grant's name when one is live.
        #[serde(default)]
        agent_name: String,
        /// What's being purchased, e.g. "CEC Diagnostic Session".
        #[serde(default)]
        item: String,
        /// Display price, e.g. "$50". The checkout page is authoritative.
        #[serde(default)]
        price: String,
        /// Optional free-text from the technician, shown under the prompt.
        #[serde(default)]
        note: String,
    },
    /// Customer → technician: where the customer is in the flow.
    Status {
        purchase_id: String,
        state: PurchaseState,
    },
    /// Technician → customer: order verified in the store — all set. The
    /// customer's prompt turns into a "confirmed, continuing" note.
    Confirm { purchase_id: String },
    /// Technician → customer: withdraw the ask (never mind / wrong click /
    /// taking payment another way). Dismisses the customer's prompt.
    Cancel { purchase_id: String },
    /// Forward-compat: an unrecognised kind decodes here and is ignored.
    #[serde(other)]
    Unknown,
}

/// The single point-to-point control envelope, dispatched on the outer `t`
/// tag. Mirrors AllMyStuff's `ControlMessage` shape, trimmed to what CEC
/// Support uses.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "t")]
pub enum ControlMessage {
    /// The connect handshake.
    Connect(ConnectControl),
    /// App/service control.
    App(AppControl),
    /// The mid-session purchase handshake. An older peer decodes this whole
    /// envelope to [`Unknown`](ControlMessage::Unknown) and ignores it — which
    /// is why [`PurchaseState::Seen`] exists: no `Seen` back means the other
    /// side may be too old to know about purchases.
    Purchase(PurchaseControl),
    /// Forward-compat catch-all.
    #[serde(other)]
    Unknown,
}

fn default_protocol() -> u32 {
    PROTOCOL_VERSION
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presence_round_trips() {
        let mut p = SupportPresence::new("device-alpha", Role::Client);
        p.label = "Reception PC".into();
        p.hostname = "reception".into();
        let json = serde_json::to_string(&p).unwrap();
        let back: SupportPresence = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
        assert_eq!(
            back.support_id,
            crate::ids::support_id_from_device("device-alpha")
        );
    }

    #[test]
    fn presence_tolerates_missing_fields() {
        // Only the required `device_id`; everything else must default.
        let p: SupportPresence = serde_json::from_str(r#"{"device_id":"d"}"#).unwrap();
        assert_eq!(p.protocol, PROTOCOL_VERSION);
        assert!(p.available);
        assert_eq!(p.role, Role::Client);
    }

    #[test]
    fn control_message_round_trips_each_variant() {
        let msgs = vec![
            ControlMessage::Connect(ConnectControl::Request {
                session_id: "s1".into(),
                agent_name: "Alex at CEC".into(),
                want_control: true,
            }),
            ControlMessage::Connect(ConnectControl::Approve {
                session_id: "s1".into(),
                scope: ApprovalScope::ThreeHours,
            }),
            ControlMessage::Connect(ConnectControl::Deny {
                session_id: "s1".into(),
                reason: "busy".into(),
            }),
            ControlMessage::Connect(ConnectControl::End {
                session_id: "s1".into(),
            }),
            ControlMessage::App(AppControl::InstallService),
        ];
        for m in msgs {
            let json = serde_json::to_string(&m).unwrap();
            let back: ControlMessage = serde_json::from_str(&json).unwrap();
            assert_eq!(m, back, "round-trip {json}");
        }
    }

    #[test]
    fn unknown_variants_decode_to_unknown() {
        let cm: ControlMessage = serde_json::from_str(r#"{"t":"someday","x":1}"#).unwrap();
        assert_eq!(cm, ControlMessage::Unknown);
        let cc: ConnectControl =
            serde_json::from_str(r#"{"kind":"renegotiate","session_id":"s"}"#).unwrap();
        assert_eq!(cc, ConnectControl::Unknown);
        let ac: AppControl = serde_json::from_str(r#"{"kind":"reboot_bios"}"#).unwrap();
        assert_eq!(ac, AppControl::Unknown);
        let pc: PurchaseControl =
            serde_json::from_str(r#"{"kind":"refund","purchase_id":"p"}"#).unwrap();
        assert_eq!(pc, PurchaseControl::Unknown);
        let ps: PurchaseState = serde_json::from_str(r#"{"kind":"escrowed"}"#).unwrap();
        assert_eq!(ps, PurchaseState::Unknown);
    }

    #[test]
    fn purchase_round_trips_each_variant() {
        let msgs = vec![
            ControlMessage::Purchase(PurchaseControl::Request {
                purchase_id: "p1".into(),
                session_id: "s1".into(),
                agent_name: "Alex at CEC".into(),
                item: "CEC Diagnostic Session".into(),
                price: "$50".into(),
                note: "So we can dig into the blue screens.".into(),
            }),
            // A sessionless ask — made before answering a help call, or after
            // disconnecting.
            ControlMessage::Purchase(PurchaseControl::Request {
                purchase_id: "p2".into(),
                session_id: String::new(),
                agent_name: "Alex at CEC".into(),
                item: "CEC Diagnostic Session".into(),
                price: "$50".into(),
                note: String::new(),
            }),
            ControlMessage::Purchase(PurchaseControl::Status {
                purchase_id: "p1".into(),
                state: PurchaseState::Seen,
            }),
            ControlMessage::Purchase(PurchaseControl::Status {
                purchase_id: "p1".into(),
                state: PurchaseState::Opened,
            }),
            ControlMessage::Purchase(PurchaseControl::Status {
                purchase_id: "p1".into(),
                state: PurchaseState::Claimed,
            }),
            ControlMessage::Purchase(PurchaseControl::Status {
                purchase_id: "p1".into(),
                state: PurchaseState::Declined,
            }),
            ControlMessage::Purchase(PurchaseControl::Confirm {
                purchase_id: "p1".into(),
            }),
            ControlMessage::Purchase(PurchaseControl::Cancel {
                purchase_id: "p1".into(),
            }),
        ];
        for m in msgs {
            let json = serde_json::to_string(&m).unwrap();
            let back: ControlMessage = serde_json::from_str(&json).unwrap();
            assert_eq!(m, back, "round-trip {json}");
        }
    }

    #[test]
    fn purchase_request_tolerates_missing_display_fields() {
        // An older (or minimal) technician sends only the ids; the display
        // strings must default to empty so the customer's app falls back to
        // its own built-in "CEC Diagnostic Session — $50" copy.
        let pc: PurchaseControl =
            serde_json::from_str(r#"{"kind":"request","purchase_id":"p1","session_id":"s1"}"#)
                .unwrap();
        assert_eq!(
            pc,
            PurchaseControl::Request {
                purchase_id: "p1".into(),
                session_id: "s1".into(),
                agent_name: String::new(),
                item: String::new(),
                price: String::new(),
                note: String::new(),
            }
        );
    }

    #[test]
    fn purchase_wire_form_is_tagged() {
        // Pin the exact wire shape: outer `t`, inner `kind`, tagged state —
        // the JSON contract the node relays and the GUIs consume.
        let m = ControlMessage::Purchase(PurchaseControl::Status {
            purchase_id: "p1".into(),
            state: PurchaseState::Claimed,
        });
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(
            json,
            r#"{"t":"purchase","kind":"status","purchase_id":"p1","state":{"kind":"claimed"}}"#
        );
    }

    #[test]
    fn approval_scope_persistence_and_expiry() {
        assert!(!ApprovalScope::Once.persists());
        assert!(ApprovalScope::ThreeHours.persists());
        assert!(ApprovalScope::Forever.persists());

        assert_eq!(ApprovalScope::Once.expires_at(1000), None);
        assert_eq!(ApprovalScope::Forever.expires_at(1000), None);
        assert_eq!(
            ApprovalScope::ThreeHours.expires_at(1000),
            Some(1000 + crate::THREE_HOURS_SECS)
        );
    }

    #[test]
    fn approval_scope_wire_form_is_tagged() {
        let json = serde_json::to_string(&ApprovalScope::ThreeHours).unwrap();
        assert_eq!(json, r#"{"kind":"three_hours"}"#);
    }
}
