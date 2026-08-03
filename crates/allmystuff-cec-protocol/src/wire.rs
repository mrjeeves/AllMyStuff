//! Peer-to-peer message types for CEC Support.
//!
//! Two things ride the mesh: a [`SupportPresence`] beacon (broadcast, so a
//! technician can find a customer), and point-to-point [`ControlMessage`]s
//! (the connect-request → approve/deny → end handshake, plus app control).
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
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

// Deserialize is hand-written (not derived) so a scope reads from BOTH the
// current internally-tagged object `{"kind":"three_hours"}` AND a bare string
// `"three_hours"`. An earlier build persisted the scope as a bare string; the
// tagged-only derive rejected those files, and because the consent store used
// to fail its whole load on one unreadable grant, that silently wiped a
// customer's standing approvals across a restart (the "can't reuse the
// approval" bug). Serialization stays tagged (unchanged on the wire).
impl<'de> Deserialize<'de> for ApprovalScope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Bare(String),
            Tagged { kind: String },
        }
        let word = match Raw::deserialize(deserializer)? {
            Raw::Bare(s) => s,
            Raw::Tagged { kind } => kind,
        };
        match word.as_str() {
            "once" => Ok(ApprovalScope::Once),
            "three_hours" => Ok(ApprovalScope::ThreeHours),
            "forever" => Ok(ApprovalScope::Forever),
            other => Err(serde::de::Error::unknown_variant(
                other,
                &["once", "three_hours", "forever"],
            )),
        }
    }
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
    /// `want_control` distinguishes view-only from full keyboard/mouse control;
    /// `want_elevated` asks for **administrator reach** on top of control (see
    /// [`Capability::Elevated`] in `allmystuff-cec-consent`), which is what
    /// lets the session drive Event Viewer, Services, Device Manager and the
    /// rest of Windows' elevated repair tooling.
    ///
    /// `want_elevated` defaults to `false`, so a technician build that predates
    /// it asks for exactly what it always asked for, and a customer node that
    /// predates it ignores the field and grants exactly what it always granted.
    /// Escalation is never something a version skew can imply.
    Request {
        session_id: String,
        #[serde(default)]
        agent_name: String,
        #[serde(default)]
        want_control: bool,
        #[serde(default)]
        want_elevated: bool,
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

/// Why administrator reach is not in force on the customer's machine, so the
/// technician's app can say something true instead of leaving them clicking a
/// dead Event Viewer wondering which end is broken.
///
/// The distinction that matters in the field is **not granted** (the customer
/// can fix it — ask them to approve admin access) versus **not effective** (the
/// customer can't fix it from the prompt; the agent isn't installed in a way
/// that can escalate, and it needs the background service).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ElevationBlocker {
    /// Nothing is blocking it — admin reach is live.
    #[default]
    None,
    /// Administrator access is switched off for this PC as a whole — either the
    /// customer declined it when the background service was installed, or they
    /// have since turned it off in Settings. Nothing about *this* session can
    /// change that; it is one switch in one place.
    NotAllowedOnThisMachine,
    /// The machine permits admin access, but this technician's grant is
    /// view-only or plain control. Reconnect asking for it.
    NotGranted,
    /// Granted, but this agent process runs as the logged-in user at medium
    /// integrity: Windows (UIPI) discards its input into elevated windows and
    /// it cannot see the secure desktop. Needs the background service, which
    /// runs as `LocalSystem` and can host the session at the required posture.
    AgentNotPrivileged,
    /// Granted, but no privileged background service is installed to escalate
    /// through. The customer installs it from Settings (one UAC prompt).
    ServiceMissing,
    /// This isn't Windows — there is no UAC/UIPI split to escalate across, so
    /// the whole question is moot and control is already whatever the OS allows.
    NotApplicable,
    /// Forward-compat: an unrecognised reason decodes here.
    #[serde(other)]
    Unknown,
}

impl ElevationBlocker {
    /// A sentence for the technician's session banner.
    pub fn detail(self) -> &'static str {
        match self {
            ElevationBlocker::None => "Administrator access is active on this session.",
            ElevationBlocker::NotAllowedOnThisMachine => {
                "Administrator access is switched off for this PC. The customer can turn it on in CEC Support → Settings."
            }
            ElevationBlocker::NotGranted => {
                "This session was approved for control only. Reconnect asking for administrator access."
            }
            ElevationBlocker::AgentNotPrivileged => {
                "CEC Support is running without administrator rights, so Windows blocks input to elevated windows. Ask the customer to install the CEC Support background service."
            }
            ElevationBlocker::ServiceMissing => {
                "The CEC Support background service isn't installed, so this session can't reach elevated windows. Ask the customer to install it from Settings."
            }
            ElevationBlocker::NotApplicable => {
                "This machine isn't Windows — control is already at the level the OS allows."
            }
            ElevationBlocker::Unknown => "Administrator access is unavailable for an unknown reason.",
        }
    }
}

/// The customer's node telling the technician what administrator reach is
/// actually in force, carried inside [`ControlMessage::Elevation`]. Sent when a
/// session starts and again whenever the answer changes (a grant approved or
/// revoked mid-session, the service installed).
///
/// It is purely **informational** — a status readout for the technician's UI.
/// Nothing on the customer side trusts it and no gate reads it; enforcement is
/// always the consent store, re-checked per frame on the machine that owns the
/// screen. A peer that lies here only lies to its own operator's status line.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ElevationReport {
    /// The session this describes.
    #[serde(default)]
    pub session_id: String,
    /// Whether the customer's live grant covers administrator reach.
    #[serde(default)]
    pub granted: bool,
    /// Whether the agent can actually deliver it right now — granted *and*
    /// running at a posture Windows will honour. This is the one the banner
    /// should key on; `granted` alone will strand a technician who was approved
    /// but is still being dropped by UIPI.
    #[serde(default)]
    pub effective: bool,
    /// Why not, when `effective` is false.
    #[serde(default)]
    pub blocker: ElevationBlocker,
}

impl ElevationReport {
    /// Build a report for `session_id` from the two facts the node knows.
    pub fn new(session_id: impl Into<String>, granted: bool, blocker: ElevationBlocker) -> Self {
        ElevationReport {
            session_id: session_id.into(),
            granted,
            effective: granted && blocker == ElevationBlocker::None,
            blocker,
        }
    }

    /// The sentence to show the technician.
    pub fn detail(&self) -> &'static str {
        self.blocker.detail()
    }
}

/// A free-text chat message exchanged between the connected technician and
/// customer, carried inside [`ControlMessage::Chat`] on
/// [`CHANNEL_CONTROL`](crate::CHANNEL_CONTROL) while a session is live. It is
/// additive: a peer built before chat existed decodes the envelope to
/// [`ControlMessage::Unknown`] and ignores it, so turning chat on never breaks
/// a mixed-version session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    /// Sender-assigned id, unique within a session — lets the receiver dedupe
    /// and lets the sender recognise the echo of its own message.
    pub id: String,
    /// Who sent it, so the UI renders the correct side without inferring it
    /// from the transport peer.
    pub from: Role,
    /// The message body. Plain UTF-8; display-escaping is the UI's job.
    pub text: String,
    /// Unix seconds when the sender composed it (sender's clock — display
    /// ordering only, never trusted for security).
    #[serde(default)]
    pub ts: u64,
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
    /// A chat message between the connected technician and customer, live only
    /// while a session is active.
    Chat(ChatMessage),
    /// Customer → technician: what administrator reach is in force, and why not
    /// when it isn't. Additive — a technician build that predates it decodes the
    /// envelope to [`ControlMessage::Unknown`] and ignores it, exactly like chat.
    Elevation(ElevationReport),
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
                want_elevated: true,
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
            ControlMessage::Chat(ChatMessage {
                id: "m1".into(),
                from: Role::Technician,
                text: "Can you close the browser and re-open it?".into(),
                ts: 1_700_000_000,
            }),
            ControlMessage::Elevation(ElevationReport::new(
                "s1",
                true,
                ElevationBlocker::ServiceMissing,
            )),
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

    // ---- elevation ----------------------------------------------------------

    #[test]
    fn an_older_technicians_request_never_asks_for_elevation() {
        // The skew that must not escalate: a Request from a build that predates
        // `want_elevated` has no such field, and must read as "not asked for".
        let req: ConnectControl = serde_json::from_str(
            r#"{"kind":"request","session_id":"s1","agent_name":"Alex","want_control":true}"#,
        )
        .unwrap();
        match req {
            ConnectControl::Request {
                want_control,
                want_elevated,
                ..
            } => {
                assert!(want_control);
                assert!(!want_elevated, "a missing field must never imply admin");
            }
            other => panic!("decoded to {other:?}"),
        }
    }

    #[test]
    fn elevation_report_is_effective_only_when_granted_and_unblocked() {
        let live = ElevationReport::new("s1", true, ElevationBlocker::None);
        assert!(live.effective);

        // Granted but the agent can't deliver it: not effective.
        let stranded = ElevationReport::new("s1", true, ElevationBlocker::AgentNotPrivileged);
        assert!(stranded.granted);
        assert!(!stranded.effective);

        // Not granted: not effective, whatever the posture.
        let ungranted = ElevationReport::new("s1", false, ElevationBlocker::None);
        assert!(!ungranted.effective);
    }

    #[test]
    fn every_blocker_explains_itself() {
        for b in [
            ElevationBlocker::None,
            ElevationBlocker::NotAllowedOnThisMachine,
            ElevationBlocker::NotGranted,
            ElevationBlocker::AgentNotPrivileged,
            ElevationBlocker::ServiceMissing,
            ElevationBlocker::NotApplicable,
            ElevationBlocker::Unknown,
        ] {
            assert!(
                !b.detail().is_empty(),
                "{b:?} has no technician-facing text"
            );
        }
    }

    #[test]
    fn unknown_blocker_and_missing_report_fields_decode() {
        // A newer customer node naming a reason this build doesn't know.
        let r: ElevationReport =
            serde_json::from_str(r#"{"session_id":"s","granted":true,"blocker":"quantum_uac"}"#)
                .unwrap();
        assert_eq!(r.blocker, ElevationBlocker::Unknown);
        // And a report carrying nothing at all still decodes to a safe default.
        let empty: ElevationReport = serde_json::from_str("{}").unwrap();
        assert!(!empty.granted);
        assert!(!empty.effective);
        assert_eq!(empty.blocker, ElevationBlocker::None);
    }
}
