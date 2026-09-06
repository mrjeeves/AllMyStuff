//! CEC Support — the technician/customer state layered on the shared mesh
//! engine.
//!
//! CEC Support is AnyDesk-style remote help built on the very same [`Mesh`]
//! engine AllMyStuff already runs (presence, the route offer/accept handshake,
//! screen `display` + `input`, the media planes, and the per-frame
//! authorization gate). The only substitution is *trust*: where an ordinary
//! AllMyStuff route is gated on owner/fleet membership
//! ([`crate::mesh::Mesh::sender_may_control`]), a CEC route is gated on the
//! customer holding a **live consent grant** for the technician
//! ([`allmystuff_cec_consent`]) — so a revoke ("Forget this technician") bites
//! immediately, mid-session, exactly like AllMyStuff re-checks authorization
//! per frame.
//!
//! Two roles share this one struct. Customers announce in the public support
//! directory while actual transports use customer-specific session rooms:
//!  * a **customer** (the standalone CEC Support client) fills
//!    [`CecInner::consent`] + [`CecInner::pending`];
//!  * a **technician** (this AllMyStuff install) fills `agent_name` +
//!    [`CecInner::dialed`].
//!
//! Everything here is plain, lock-guarded state plus pure helpers, so the wire
//! contract ([`allmystuff_cec_protocol`]) and the enforcement store
//! ([`allmystuff_cec_consent`]) stay the single sources of truth — this module
//! only *bookkeeps* which technicians are pending/dialed and projects that into
//! JSON for the node-control surface and the `cec://*` events.
//!
//! [`Mesh`]: crate::mesh::Mesh

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use allmystuff_cec_consent::{capabilities_for, Capability, ConsentStore};
use allmystuff_cec_protocol::{
    format_support_id, support_id_from_device, ApprovalScope, ChatMessage, Role,
};

/// Where the customer's consent store lives:
/// `~/.myownmesh/cec-consent.json`, honouring `MYOWNMESH_HOME` — the same home
/// the ownership store and control socket use. `None` (no home resolvable) runs
/// the store in memory.
pub fn consent_store_path() -> Option<PathBuf> {
    Some(allmystuff_protocol::myownmesh_state_dir()?.join("cec-consent.json"))
}

/// This machine's wall clock as Unix seconds — the injected `now` the consent
/// store enforces expiry against (it never reads the clock itself).
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One inbound technician connect-request awaiting the customer's 3-choice
/// prompt (customer side). Built from a [`ConnectControl::Request`] arriving on
/// the `cec.control` channel; surfaced verbatim in `cec_pending` and the
/// `cec://request` event.
///
/// [`ConnectControl::Request`]: allmystuff_cec_protocol::ConnectControl::Request
#[derive(Clone, Debug)]
pub struct PendingRequest {
    /// The technician's device id (as it arrived — display or bare).
    pub tech: String,
    /// The Agent Name the customer sees ("*so-and-so* is trying to connect").
    pub agent_name: String,
    /// Whether the technician asked for keyboard/mouse control (vs view-only).
    pub want_control: bool,
    /// The session id the technician minted for this attempt.
    pub session_id: String,
    /// A short, human-comparable code the customer can read back to confirm the
    /// technician out-of-band before approving.
    pub verification_code: String,
}

impl PendingRequest {
    fn to_value(&self) -> Value {
        json!({
            "tech": self.tech,
            "agent_name": self.agent_name,
            "want_control": self.want_control,
            "session_id": self.session_id,
            "verification_code": self.verification_code,
        })
    }
}

/// One customer this technician has dialed (technician side). Keyed in
/// [`CecInner::dialed`] by the customer's canonical (bare-pubkey) id. A dialed
/// customer is an ordinary mesh peer on the graph — the CEC tab lists these
/// from CEC state ([`Cec::dialed_list`]), it is not a graph grouping.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DialedCustomer {
    /// The customer's device id — the directory key (canonical form) and the
    /// address every deliberate session-room dial targets.
    pub node: String,
    /// The customer's support number — a display/verification label derived
    /// from their device key, kept so the technician's card and the
    /// customer's app spell the same digits. It also deterministically names
    /// the isolated session room, without exposing a separate user concept.
    pub number: String,
    /// Best-known label (the machine name, once presence lands).
    pub label: String,
    /// Best-known hostname, shown beside the label so the technician's card
    /// spells the same identity the customer's app shows. `default` so a
    /// directory persisted by an older build still loads.
    #[serde(default)]
    pub hostname: String,
    /// Whether the customer is currently reachable.
    pub online: bool,
    /// Epoch seconds of the last time the technician actively used this
    /// connection — a fresh dial, or the console session going active. Surfaced
    /// to the CEC tab so a technician can spot (and clean up) connections gone
    /// stale while keeping the ones they've reached for recently.
    pub last_used: u64,
}

impl DialedCustomer {
    /// The `cec://peer` / `cec_dial` result shape.
    pub fn to_value(&self) -> Value {
        json!({
            "node": self.node,
            "number": self.number,
            "label": self.label,
            "hostname": self.hostname,
            "online": self.online,
            "last_used": self.last_used,
        })
    }
}

/// Load the persisted dialed-customer directory, keyed by each customer's
/// canonical (bare-pubkey) id. `online` is reset to `false` — reachability is
/// re-confirmed live (see the `cec_dialed` reconcile), never trusted from a
/// prior run. A missing or corrupt file loads empty; it never bricks the node.
///
/// Migration: older directories could be number-keyed and hold nodeless
/// "attempt" rows. A nodeless row cannot identify a customer, so it is dropped
/// on load; rows with a node id re-key by it losslessly.
/// (Old rows also carried a `network_id` room field; serde ignores it.)
fn load_dialed(path: Option<&PathBuf>) -> HashMap<String, DialedCustomer> {
    let Some(path) = path else {
        return HashMap::new();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    let list: Vec<DialedCustomer> = serde_json::from_str(&text).unwrap_or_default();
    list.into_iter()
        .filter(|c| !c.node.is_empty())
        .map(|mut c| {
            c.online = false;
            (pubkey_part(&c.node).to_string(), c)
        })
        .collect()
}

/// The persisted shape of the per-peer chat transcripts: a version tag plus the
/// map of canonical-peer-id → messages (oldest-first). Versioned like the
/// consent store so a future format change is a migration, never a silent wipe.
#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedChats {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    chats: HashMap<String, Vec<ChatMessage>>,
}

/// Current on-disk version of the chat transcript store.
const CHAT_STORE_VERSION: u32 = 1;

/// How many of the most recent messages a single peer's transcript keeps. A
/// live chat is small, but the store is durable across sessions, so cap each
/// peer so an old relationship's file can't grow without bound — the oldest
/// lines fall off first.
const CHAT_HISTORY_CAP: usize = 500;

/// Load the persisted chat transcripts, keyed by each peer's canonical
/// (bare-pubkey) id. A missing file loads empty and a corrupt one is quarantined
/// aside by [`crate::persist::load_json`] rather than bricking the node — at
/// worst a transcript restarts empty, exactly like the consent/dialed stores.
fn load_chats(path: Option<&PathBuf>) -> HashMap<String, Vec<ChatMessage>> {
    let Some(path) = path else {
        return HashMap::new();
    };
    let loaded: PersistedChats = crate::persist::load_json(path);
    loaded.chats
}

/// The CEC state a node carries — all behind one lock, since both the
/// node-control commands and the mesh's per-frame gate reach it from many
/// tasks. Cheap to hold: the enforcement (expiry, persistence) lives in the
/// [`ConsentStore`], not here.
pub struct Cec {
    inner: Mutex<CecInner>,
    /// Where the technician's dialed-customer directory is mirrored, so it
    /// survives a node restart. The customer's Silent mesh is persisted
    /// daemon-side; this keeps the technician's view of every machine they've
    /// serviced in step (independent of grant lifetime — an expired grant just
    /// re-prompts the customer on the next connect). `None` for an in-memory
    /// node (tests), where nothing is written.
    dialed_path: Option<PathBuf>,
    /// Where the per-peer chat transcripts are mirrored (`cec-chats.json`),
    /// alongside the consent + dialed stores under the same node home. `None`
    /// for an in-memory node (tests), where nothing is written.
    chats_path: Option<PathBuf>,
}

struct CecInner {
    /// Which side this node is playing. Defaults to [`Role::Client`] (the CEC
    /// Support app's role); flips to [`Role::Technician`] on the first
    /// `cec_dial`.
    role: Role,
    /// This node's own support number (customer) — derived once the local id is
    /// known. Empty until then. It is the customer's display/verification label
    /// and the stable input used to derive their isolated session-room id.
    number: String,
    /// The technician's **Agent Name** — the name a customer sees in the prompt.
    /// Persisted GUI-side; mirrored here so an outbound connect-request carries
    /// it.
    agent_name: String,
    /// The customer's standing approvals — the enforcement store consulted on
    /// every privileged CEC frame.
    consent: ConsentStore,
    /// Inbound connect-requests awaiting the customer's decision.
    pending: Vec<PendingRequest>,
    /// Every client machine this technician has *attempted* (number digits →
    /// record) — a permanent directory row exists from the moment of the dial,
    /// whether or not the customer ever answered. `node` is filled in once
    /// discovery succeeds. Surfaced to the CEC tab via [`Cec::dialed_list`];
    /// rows leave only via the explicit forget/curate actions.
    dialed: HashMap<String, DialedCustomer>,
    /// Live session states by session id, for `cec://session`.
    sessions: HashMap<String, String>,
    /// Which technician (canonical id) each active session belongs to —
    /// populated when the customer approves (or auto-approves) a session, so
    /// the consent sweep can end exactly that technician's sessions when their
    /// grant lapses. Keyed by session id, like [`Self::sessions`]; customer
    /// side only.
    session_tech: HashMap<String, String>,
    /// Sessions whose Approve frame the technician has acknowledged receiving.
    /// KVM passthrough must not start from local approval alone: until the far
    /// side has consumed Approve it rejects the customer's KVM announcement as
    /// coming from a non-active session, and the appliance's immediate greet is
    /// then lost behind its cooldown. Memory-only, like the sessions themselves.
    kvm_support_ready: HashSet<String>,
    /// KVMs temporarily exposed by a customer this technician is actively
    /// controlling. These are deliberately memory-only leases: the customer's
    /// 2-second heartbeat renews them, and a dead app/session loses them within
    /// a few seconds instead of creating unattended appliance access.
    support_kvms: HashMap<String, SupportKvm>,
    /// Cancellation flag for the in-flight dial (one at a time — the GUI
    /// serializes dials). `begin_dial` mints a fresh flag; `cancel_dial` trips
    /// it; the discovery poll and the connect-request re-send loop both honor
    /// it, so "stop trying" actually stops everything being tried.
    dial_cancel: Option<Arc<AtomicBool>>,
    /// Per-peer chat transcripts (both roles), keyed by the peer's canonical
    /// (bare-pubkey) id, each oldest-first and capped at [`CHAT_HISTORY_CAP`].
    /// Holds both received lines and the echoes of ones this node sent, so a
    /// peer's history is the whole conversation. Mirrored to `chats_path`.
    chats: HashMap<String, Vec<ChatMessage>>,
}

struct SupportKvm {
    customer: String,
    until: Instant,
}

impl Cec {
    /// Build the CEC state, loading (or, with `None`, running an in-memory)
    /// consent store. The store path is a `consent.json` under the node's home
    /// — a corrupt or absent file loads empty (it never bricks the node), and
    /// only `ThreeHours`/`Forever` grants are ever written.
    pub fn new(consent_path: Option<PathBuf>) -> Self {
        // Mirror the dialed directory next to the consent store, under the same
        // node home. A `None` consent path (tests) means no persistence.
        let dialed_path = consent_path
            .as_ref()
            .and_then(|p| p.parent())
            .map(|dir| dir.join("cec-dialed.json"));
        // The chat transcripts live beside the consent + dialed stores under the
        // same node home, on the same tolerant-load / atomic-write discipline.
        let chats_path = consent_path
            .as_ref()
            .and_then(|p| p.parent())
            .map(|dir| dir.join("cec-chats.json"));
        let consent = match consent_path {
            Some(p) => ConsentStore::load(p),
            None => ConsentStore::in_memory(),
        };
        let dialed = load_dialed(dialed_path.as_ref());
        let chats = load_chats(chats_path.as_ref());
        Cec {
            dialed_path,
            chats_path,
            inner: Mutex::new(CecInner {
                role: Role::Client,
                number: String::new(),
                agent_name: String::new(),
                consent,
                pending: Vec::new(),
                dialed,
                sessions: HashMap::new(),
                session_tech: HashMap::new(),
                kvm_support_ready: HashSet::new(),
                support_kvms: HashMap::new(),
                dial_cancel: None,
                chats,
            }),
        }
    }

    /// Mirror the dialed directory to disk. Best-effort: a write failure warns
    /// and is dropped (an in-memory list still beats bricking on a read-only
    /// disk). Called after every mutation of `dialed`.
    fn persist_dialed(&self, list: Vec<DialedCustomer>) {
        let Some(path) = &self.dialed_path else {
            return;
        };
        match serde_json::to_string_pretty(&list) {
            Ok(json) => {
                if let Err(e) = std::fs::write(path, json) {
                    tracing::warn!(
                        "couldn't persist CEC dialed customers to {}: {e}",
                        path.display()
                    );
                }
            }
            Err(e) => tracing::warn!("couldn't serialize CEC dialed customers: {e}"),
        }
    }

    // ---- chat transcript (both roles) -----------------------------------

    /// Mirror the chat transcripts to disk atomically (temp + fsync + rename,
    /// 0600 on Unix) — the same crash-safe discipline the consent store uses, so
    /// a half-written transcript can never replace a good one. Best-effort: a
    /// write failure warns and is dropped (the in-memory log still serves the
    /// live session). Called after every append.
    fn persist_chats(&self, chats: HashMap<String, Vec<ChatMessage>>) {
        let Some(path) = &self.chats_path else {
            return;
        };
        let doc = PersistedChats {
            version: CHAT_STORE_VERSION,
            chats,
        };
        match serde_json::to_vec_pretty(&doc) {
            Ok(bytes) => {
                if let Err(e) = crate::persist::write_atomic(path, &bytes) {
                    tracing::warn!(
                        "couldn't persist CEC chat transcripts to {}: {e}",
                        path.display()
                    );
                }
            }
            Err(e) => tracing::warn!("couldn't serialize CEC chat transcripts: {e}"),
        }
    }

    /// Append one message to `peer`'s transcript (canonicalised key) and persist.
    /// Used for both an inbound receive and the echo of a line this node sent, so
    /// a peer's history holds both halves of the conversation oldest-first. The
    /// transcript is capped at [`CHAT_HISTORY_CAP`] — the oldest lines fall off
    /// so a durable file can't grow without bound.
    pub fn push_chat(&self, peer: &str, msg: ChatMessage) {
        let key = pubkey_part(peer).to_string();
        let mut inner = self.inner.lock();
        let log = inner.chats.entry(key).or_default();
        log.push(msg);
        if log.len() > CHAT_HISTORY_CAP {
            let excess = log.len() - CHAT_HISTORY_CAP;
            log.drain(0..excess);
        }
        // Snapshot under the lock, persist outside it (write_atomic does disk
        // I/O — never hold the state lock across a syscall), mirroring how the
        // dialed directory is persisted.
        let snapshot = inner.chats.clone();
        drop(inner);
        self.persist_chats(snapshot);
    }

    /// `peer`'s stored transcript (canonicalised key), oldest-first — what
    /// `cec_chat_history` projects for the GUI. Empty for a peer never chatted.
    pub fn chat_history(&self, peer: &str) -> Vec<ChatMessage> {
        let key = pubkey_part(peer);
        self.inner
            .lock()
            .chats
            .get(key)
            .cloned()
            .unwrap_or_default()
    }

    // ---- status ---------------------------------------------------------

    /// The `cec_status` result: `{ number, network_id, role, asking_help }`.
    /// When `me` is known and this node has no number yet, derive its own from
    /// it so a customer can read its code straight away. `network_id` names
    /// the public directory for compatibility with existing clients.
    pub fn status(&self, me: Option<&str>) -> Value {
        let mut inner = self.inner.lock();
        if inner.number.is_empty() {
            if let Some(me) = me {
                inner.number = support_id_from_device(me);
            }
        }
        json!({
            "number": inner.number,
            "network_id": allmystuff_cec_protocol::HELP_NETWORK_ID,
            "role": role_str(inner.role),
            "asking_help": false,
        })
    }

    // ---- technician (Agent Name + dial bookkeeping) ---------------------

    /// The technician's Agent Name, for stamping an outbound connect-request.
    pub fn agent_name(&self) -> String {
        self.inner.lock().agent_name.clone()
    }

    /// Set (persist mirror of) the technician's Agent Name.
    pub fn set_agent_name(&self, name: String) {
        self.inner.lock().agent_name = name;
    }

    /// Note that this node is now acting as a technician (first dial).
    pub fn note_technician(&self) {
        self.inner.lock().role = Role::Technician;
    }

    /// Record (or refresh) a dialed customer, keyed by canonical id. Returns the
    /// stored record for the `cec://peer` emit.
    pub fn record_dialed(
        &self,
        node: String,
        number: String,
        label: String,
        hostname: String,
        online: bool,
    ) -> DialedCustomer {
        let now = now_secs();
        let mut inner = self.inner.lock();
        let entry = inner
            .dialed
            .entry(pubkey_part(&node).to_string())
            .or_insert_with(|| DialedCustomer {
                node: node.clone(),
                number: number.clone(),
                label: label.clone(),
                hostname: hostname.clone(),
                online,
                last_used: now,
            });
        entry.node = node;
        entry.number = number;
        if !label.is_empty() {
            entry.label = label;
        }
        if !hostname.is_empty() {
            entry.hostname = hostname;
        }
        entry.online = online;
        // A (re)dial is a fresh use — keep the stale-connection metric honest.
        entry.last_used = now;
        let record = entry.clone();
        let snapshot: Vec<DialedCustomer> = inner.dialed.values().cloned().collect();
        drop(inner);
        self.persist_dialed(snapshot);
        record
    }

    /// End every live session except `keep`, returning the ids that changed so
    /// the caller can emit their `cec://session` transitions. The customer flow
    /// is one live support session at a time: a technician's re-dial mints a
    /// fresh session id, and without this the old rows stack up in the
    /// customer's "viewing your screen" banner forever.
    pub fn end_other_sessions(&self, keep: &str) -> Vec<String> {
        let mut inner = self.inner.lock();
        let ended: Vec<String> = inner
            .sessions
            .iter()
            .filter(|(id, state)| id.as_str() != keep && state.as_str() == "active")
            .map(|(id, _)| id.clone())
            .collect();
        for id in &ended {
            inner.sessions.insert(id.clone(), "ended".to_string());
            inner.session_tech.remove(id);
            inner.kvm_support_ready.remove(id);
        }
        ended
    }

    /// Mint the cancellation flag for a new dial, replacing any stale one.
    /// The returned flag is checked by every loop the dial runs.
    pub fn begin_dial(&self) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        self.inner.lock().dial_cancel = Some(flag.clone());
        flag
    }

    /// Trip the in-flight dial's cancellation flag ("stop trying"). Harmless
    /// when nothing is in flight — a completed dial's stale flag has no
    /// readers left.
    pub fn cancel_dial(&self) {
        if let Some(f) = &self.inner.lock().dial_cancel {
            f.store(true, Ordering::Relaxed);
        }
    }

    /// Mark a dialed customer online/offline (from presence), returning its
    /// updated record when the flag actually changed.
    pub fn set_customer_online(&self, canonical: &str, online: bool) -> Option<DialedCustomer> {
        // `online` is ephemeral — reconciled live and reset to false on load — so
        // this stays in memory; only the durable fields (record/last_used) are
        // ever written to disk.
        let mut inner = self.inner.lock();
        let c = inner.dialed.get_mut(canonical)?;
        if c.online == online {
            return None;
        }
        c.online = online;
        Some(c.clone())
    }

    /// Stamp a dialed customer as just-used (`last_used = now`) — called when the
    /// console session with them goes active, so the CEC tab's "last used"
    /// reflects real activity, not just the original dial. Returns the updated
    /// record (for a `cec://peer` re-emit); `None` for a customer we haven't
    /// dialed.
    pub fn touch_dialed(&self, canonical: &str) -> Option<DialedCustomer> {
        let mut inner = self.inner.lock();
        let c = inner.dialed.get_mut(canonical)?;
        c.last_used = now_secs();
        let record = c.clone();
        let snapshot: Vec<DialedCustomer> = inner.dialed.values().cloned().collect();
        drop(inner);
        self.persist_dialed(snapshot);
        Some(record)
    }

    /// Whether `canonical` is a customer this technician has dialed. Used by
    /// "Forget this node" to know a CEC directory row needs dropping too.
    pub fn is_dialed(&self, canonical: &str) -> bool {
        self.inner.lock().dialed.contains_key(canonical)
    }

    /// The customers this technician has dialed, projected for the CEC tab's
    /// "Active connections" list (`cec_dialed`) — the same `{ node, number,
    /// label, online }` shape as a `cec://peer` event. Each entry is an ordinary
    /// mesh peer on the graph; the tab reads them from here rather than from any
    /// graph grouping (there is none — the CEC mesh is Silent, with no roster).
    pub fn dialed_list(&self) -> Vec<Value> {
        self.inner
            .lock()
            .dialed
            .values()
            .map(DialedCustomer::to_value)
            .collect()
    }

    /// The dialed customers as owned records, for the async `cec_dialed`
    /// projection that reconciles each one's live reachability against the
    /// daemon's peer set. [`dialed_list`] returns the UI shape.
    pub fn dialed_records(&self) -> Vec<DialedCustomer> {
        self.inner.lock().dialed.values().cloned().collect()
    }

    /// Drop a customer this technician dialed (the CEC part of "Forget this
    /// node"). Returns `true` when one was actually removed.
    pub fn forget_dialed(&self, canonical: &str) -> bool {
        let mut inner = self.inner.lock();
        let removed = inner.dialed.remove(canonical).is_some();
        if removed {
            let snapshot: Vec<DialedCustomer> = inner.dialed.values().cloned().collect();
            drop(inner);
            self.persist_dialed(snapshot);
        }
        removed
    }

    // ---- customer (the 3-choice consent flow) ---------------------------

    /// Resolve (and remember) this customer's support number for display —
    /// the digits a caller reads out on the phone. Pure derivation from the
    /// device key; no mesh state changes hands here.
    pub fn own_number(&self, me: Option<&str>) -> String {
        let mut inner = self.inner.lock();
        if inner.number.is_empty() {
            if let Some(me) = me {
                inner.number = support_id_from_device(me);
            }
        }
        inner.number.clone()
    }

    /// Record an inbound technician connect-request (customer side), replacing
    /// any prior pending attempt from the same technician so a redial doesn't
    /// stack duplicates.
    pub fn record_pending(&self, req: PendingRequest) {
        let mut inner = self.inner.lock();
        let tech = pubkey_part(&req.tech).to_string();
        inner.pending.retain(|p| pubkey_part(&p.tech) != tech);
        inner.pending.push(req);
    }

    /// The customer's pending connect-requests, for `cec_pending`.
    pub fn pending(&self) -> Vec<Value> {
        self.inner
            .lock()
            .pending
            .iter()
            .map(PendingRequest::to_value)
            .collect()
    }

    /// Look up a pending request's Agent Name (kept with the grant so the
    /// customer recognises a "Forget this technician" entry later).
    pub fn pending_agent_name(&self, tech: &str) -> String {
        let inner = self.inner.lock();
        let key = pubkey_part(tech);
        inner
            .pending
            .iter()
            .find(|p| pubkey_part(&p.tech) == key)
            .map(|p| p.agent_name.clone())
            .unwrap_or_default()
    }

    /// Record the customer's approval of `tech` at `scope` in the consent
    /// store, dropping the matching pending request. A failed durable write
    /// (a `ThreeHours`/`Forever` grant that couldn't be saved) returns the
    /// error and records nothing — never a silent security downgrade.
    pub fn approve(
        &self,
        tech: &str,
        agent_name: &str,
        scope: ApprovalScope,
        want_control: bool,
    ) -> Result<(), String> {
        let mut inner = self.inner.lock();
        inner
            .consent
            .approve(
                tech,
                agent_name,
                capabilities_for(want_control),
                scope,
                now_secs(),
            )
            .map_err(|e| e.to_string())?;
        let key = pubkey_part(tech).to_string();
        inner.pending.retain(|p| pubkey_part(&p.tech) != key);
        Ok(())
    }

    /// Drop a pending request (a plain "Deny", no grant recorded).
    pub fn deny(&self, tech: &str) {
        let mut inner = self.inner.lock();
        let key = pubkey_part(tech).to_string();
        inner.pending.retain(|p| pubkey_part(&p.tech) != key);
    }

    /// Revoke every grant for `tech` ("Forget this technician"). Returns
    /// whether anything was actually removed (so the caller can skip a teardown
    /// that isn't needed). Also drops any pending request from them.
    pub fn revoke(&self, tech: &str) -> Result<bool, String> {
        let mut inner = self.inner.lock();
        let key = pubkey_part(tech).to_string();
        inner.pending.retain(|p| pubkey_part(&p.tech) != key);
        inner.consent.revoke(tech).map_err(|e| e.to_string())
    }

    /// The customer's live grants, for `cec_grants` and the `cec://grants`
    /// event.
    pub fn grants(&self) -> Vec<Value> {
        let inner = self.inner.lock();
        inner
            .consent
            .active_grants(now_secs())
            .into_iter()
            .map(|g| {
                json!({
                    "technician": g.technician,
                    "agent_name": g.agent_name,
                    "scope": scope_str(g.scope),
                    "granted_at": g.granted_at,
                    "expires_at": g.expires_at,
                    "control": g
                        .capabilities
                        .iter()
                        .any(|c| matches!(c, Capability::Control)),
                })
            })
            .collect()
    }

    /// The per-frame enforcement check: whether `tech` currently holds a live
    /// grant covering `cap`. Consulted from the mesh's `sender_may_drive`
    /// (Control) and the screen-offer screen (ScreenView), so a revoke bites
    /// the next frame. Reads the clock via [`now_secs`] so an expired grant
    /// stops mid-session with no bookkeeping tick.
    pub fn is_allowed(&self, tech: &str, cap: Capability) -> bool {
        self.inner.lock().consent.is_allowed(tech, cap, now_secs())
    }

    /// Whether this node has *any* CEC involvement with `tech` — a pending
    /// request, a live grant, or a dialed record. Lets the mesh treat a peer as
    /// a CEC technician (gate on consent) only when CEC actually applies, so the
    /// consent path never narrows an ordinary owner/fleet peer.
    pub fn knows_technician(&self, tech: &str) -> bool {
        let inner = self.inner.lock();
        let key = pubkey_part(tech);
        // Grant records count live **or lapsed** (`ConsentStore::known`): an
        // expired technician must stay recognized, so the screen-offer gate
        // keeps screening them (lapsed ≠ stranger) and a control refusal can
        // name the lapsed approval instead of blaming the fleet roster.
        inner.pending.iter().any(|p| pubkey_part(&p.tech) == key) || inner.consent.known(tech)
    }

    /// Whether this node has ANY deliberate CEC relationship with `peer`:
    /// a consent grant (live or lapsed-but-remembered) or a pending connect
    /// request on the customer side, a dialed-directory row on the
    /// technician side. This is the app-layer visibility gate for the CEC
    /// rooms — AllMyStuff's own presence/graph protocol crosses a CEC room
    /// only between peers that pass it, so a stranger co-resident on the
    /// (world-joinable) support area never surfaces as a computer in
    /// anyone's graph, and this node's profile is never volunteered to one.
    pub fn relationship_with(&self, peer: &str) -> bool {
        let key = pubkey_part(peer);
        let mut inner = self.inner.lock();
        let now = Instant::now();
        inner.support_kvms.retain(|_, lease| lease.until > now);
        inner.dialed.contains_key(key)
            || inner.consent.known(key)
            || inner.pending.iter().any(|p| pubkey_part(&p.tech) == key)
            || inner.support_kvms.contains_key(key)
    }

    /// The customer that announced this transient support KVM, if its lease is
    /// still live. The presence gate uses this to require the appliance's own
    /// authoritative `kvm.attached_to` advert to agree with the announcement.
    pub fn support_kvm_customer(&self, kvm: &str) -> Option<String> {
        let key = pubkey_part(kvm);
        let mut inner = self.inner.lock();
        let now = Instant::now();
        inner.support_kvms.retain(|_, lease| lease.until > now);
        inner
            .support_kvms
            .get(key)
            .map(|lease| lease.customer.clone())
    }

    /// A direct CEC relationship independent of transient KVM passthrough.
    /// Directly dialed KVMs keep their normal profile rules even if they also
    /// happen to be the appliance attached to another active customer.
    pub fn direct_relationship_with(&self, peer: &str) -> bool {
        let key = pubkey_part(peer);
        let inner = self.inner.lock();
        inner.dialed.contains_key(key)
            || inner.consent.known(key)
            || inner.pending.iter().any(|p| pubkey_part(&p.tech) == key)
    }

    /// Whether `customer` currently has an active CEC session with this node.
    /// Used on the technician side to authenticate a customer's announcement
    /// that an attached KVM is available for this exact support session.
    pub fn has_active_session_with(&self, customer: &str) -> bool {
        let key = pubkey_part(customer);
        let inner = self.inner.lock();
        inner.session_tech.iter().any(|(session, peer)| {
            pubkey_part(peer) == key
                && inner
                    .sessions
                    .get(session)
                    .is_some_and(|state| state == "active")
        })
    }

    /// Customer-side technicians whose approved support sessions are active
    /// and whose Approve frame has reached the technician. This is the authority
    /// window for attached-KVM delegation: the KVM is part of the support
    /// console from bilateral session start, before the technician happens to
    /// turn on (or send the first event over) its input route.
    pub fn active_session_technicians(&self) -> Vec<String> {
        let inner = self.inner.lock();
        let mut peers: Vec<String> = inner
            .session_tech
            .iter()
            .filter(|(session, _)| {
                inner
                    .sessions
                    .get(*session)
                    .is_some_and(|state| state == "active")
                    && inner.kvm_support_ready.contains(*session)
            })
            .map(|(_, peer)| peer.clone())
            .collect();
        peers.sort();
        peers.dedup();
        peers
    }

    /// Admit one customer-announced KVM through the CEC presence filter for a
    /// short renewable lease. Returns false for malformed or implausibly long
    /// leases; callers must never allow a wire value to mint standing access.
    pub fn note_support_kvm(&self, customer: &str, kvm: &str, expires_in: u64) -> bool {
        const MAX_LEASE: u64 = 15;
        if customer.is_empty() || kvm.is_empty() || expires_in == 0 || expires_in > MAX_LEASE {
            return false;
        }
        let key = pubkey_part(kvm).to_string();
        self.inner.lock().support_kvms.insert(
            key,
            SupportKvm {
                customer: pubkey_part(customer).to_string(),
                until: Instant::now() + Duration::from_secs(expires_in),
            },
        );
        true
    }

    /// Drop expired support-KVM discovery leases and return their ids so the
    /// mesh can remove the now-inaccessible transient graph profiles.
    pub fn prune_support_kvms(&self) -> Vec<String> {
        let now = Instant::now();
        let mut inner = self.inner.lock();
        let expired: Vec<String> = inner
            .support_kvms
            .iter()
            .filter(|(_, lease)| lease.until <= now)
            .map(|(kvm, _)| kvm.clone())
            .collect();
        for kvm in &expired {
            if let Some(lease) = inner.support_kvms.remove(kvm) {
                tracing::info!(
                    "CEC support KVM {} from customer {} expired",
                    kvm,
                    lease.customer
                );
            }
        }
        expired
    }

    // ---- session state --------------------------------------------------

    /// Record a session's state, returning it for the `cec://session` emit.
    pub fn set_session(&self, session_id: &str, state: &str) {
        let mut inner = self.inner.lock();
        inner
            .sessions
            .insert(session_id.to_string(), state.to_string());
        // A session that reached a terminal state no longer needs its
        // technician binding — drop it so the sweep's `session_tech` map can't
        // accumulate stale rows across a technician's reconnects.
        if matches!(state, "ended" | "denied") {
            inner.session_tech.remove(session_id);
            inner.kvm_support_ready.remove(session_id);
        }
    }

    /// Move a session to `state` only while it is still in `expected`.
    ///
    /// Dial timeout/cancellation runs concurrently with inbound approval. A
    /// plain read followed by [`Self::set_session`] could overwrite an approval
    /// that landed between those operations, so keep the compare and write
    /// under one lock.
    pub fn transition_session(&self, session_id: &str, expected: &str, state: &str) -> bool {
        let mut inner = self.inner.lock();
        if inner.sessions.get(session_id).map(String::as_str) != Some(expected) {
            return false;
        }
        inner
            .sessions
            .insert(session_id.to_string(), state.to_string());
        if matches!(state, "ended" | "denied") {
            inner.session_tech.remove(session_id);
            inner.kvm_support_ready.remove(session_id);
        }
        true
    }

    /// Bind a session to the technician it authorises (customer side) — called
    /// when the customer approves or auto-approves, so the consent sweep can
    /// later end the right technician's sessions. The tech id is canonicalised,
    /// matching every other consent-store key. Idempotent.
    pub fn bind_session(&self, session_id: &str, tech: &str) {
        self.inner
            .lock()
            .session_tech
            .insert(session_id.to_string(), pubkey_part(tech).to_string());
    }

    /// Record the bilateral half of approval: the technician has consumed our
    /// acknowledged Approve frame. Only then may the customer introduce its
    /// attached KVM to that technician. Returns false if the session stopped
    /// being active while the delivery was in flight.
    pub fn mark_kvm_support_ready(&self, session_id: &str) -> bool {
        let mut inner = self.inner.lock();
        if inner.sessions.get(session_id).map(String::as_str) != Some("active")
            || !inner.session_tech.contains_key(session_id)
        {
            return false;
        }
        inner.kvm_support_ready.insert(session_id.to_string());
        true
    }

    /// End (and forget) every session bound to `tech`, returning their ids so
    /// the caller can emit `cec://session … ended`. Also clears their entries
    /// from the live-session map. Customer side, driven by the consent sweep
    /// when a grant lapses.
    pub fn end_sessions_for(&self, tech: &str) -> Vec<String> {
        let key = pubkey_part(tech).to_string();
        let mut inner = self.inner.lock();
        let ids: Vec<String> = inner
            .session_tech
            .iter()
            .filter(|(_, t)| **t == key)
            .map(|(sid, _)| sid.clone())
            .collect();
        for sid in &ids {
            inner.session_tech.remove(sid);
            inner.kvm_support_ready.remove(sid);
            inner.sessions.remove(sid);
        }
        ids
    }

    /// The last recorded state for a session (`requested` / `active` / `denied`
    /// / `ended`), if known. Lets the technician's dial loop stop re-sending the
    /// connect-request once the customer has answered.
    pub fn session_state(&self, session_id: &str) -> Option<String> {
        self.inner.lock().sessions.get(session_id).cloned()
    }

    /// Whether a pending connect-request is already recorded for `session_id`.
    /// The technician retransmits its Request every 2s until answered, so this
    /// lets the customer refresh the pending record on each beat *without*
    /// re-raising the approval prompt every time.
    pub fn has_pending_session(&self, session_id: &str) -> bool {
        self.inner
            .lock()
            .pending
            .iter()
            .any(|p| p.session_id == session_id)
    }

    /// The scope of the customer's live grant for `tech`, if any — used to
    /// re-send an Approve when a retransmitted Request shows the first one never
    /// reached the technician. `None` only when no grant is held; the technician
    /// ignores the scope on an Approve (it merely moves the session to active),
    /// so the caller can default it.
    pub fn active_scope_for(&self, tech: &str) -> Option<ApprovalScope> {
        let inner = self.inner.lock();
        let key = pubkey_part(tech);
        inner
            .consent
            .active_grants(now_secs())
            .into_iter()
            .find(|g| pubkey_part(&g.technician) == key)
            .map(|g| g.scope)
    }

    /// The scope of a **standing** (persistent) grant for `tech` — 3-hours or
    /// Forever only. This is the auto-approve check for a *new* session: an
    /// "Approve Once" covers exactly the session it was granted in, so it must
    /// never silently approve a reconnect — a fresh dial re-prompts instead.
    pub fn standing_scope_for(&self, tech: &str) -> Option<ApprovalScope> {
        let inner = self.inner.lock();
        let key = pubkey_part(tech);
        inner
            .consent
            .active_grants(now_secs())
            .into_iter()
            .find(|g| g.scope.persists() && pubkey_part(&g.technician) == key)
            .map(|g| g.scope)
    }

    /// Retire the in-memory "Approve Once" grant for `tech` — the session it
    /// covered ended, and Once must not outlive its session. Returns whether a
    /// grant was actually dropped (so the caller can re-emit the grant list).
    pub fn retire_once(&self, tech: &str) -> bool {
        self.inner.lock().consent.revoke_once(tech)
    }
}

/// The customer-facing scope word for a grant (the `cec_grants` shape) —
/// mirrors the wire `snake_case` (`once` / `three_hours` / `forever`).
fn scope_str(scope: ApprovalScope) -> &'static str {
    match scope {
        ApprovalScope::Once => "once",
        ApprovalScope::ThreeHours => "three_hours",
        ApprovalScope::Forever => "forever",
    }
}

/// The `cec_status` role word — `client` / `technician`.
fn role_str(role: Role) -> &'static str {
    match role {
        Role::Client => "client",
        Role::Technician => "technician",
    }
}

/// Map the node-control `scope` argument (`"once" | "three_hours" | "forever"`)
/// to an [`ApprovalScope`]. Unknown values are an error the dispatch surfaces.
pub fn parse_scope(s: &str) -> Result<ApprovalScope, String> {
    match s {
        "once" => Ok(ApprovalScope::Once),
        "three_hours" => Ok(ApprovalScope::ThreeHours),
        "forever" => Ok(ApprovalScope::Forever),
        other => Err(format!(
            "unknown approval scope '{other}' (want once | three_hours | forever)"
        )),
    }
}

/// A short, stable verification code for a connect attempt — the first 6
/// digits of the Support ID of the concatenated technician id and session id,
/// so both ends compute the same code to read back out-of-band.
pub fn verification_code(tech: &str, session_id: &str) -> String {
    // The raw string hash, NOT the device derivation: this input is
    // `tech:session`, and device-id canonicalisation must never touch it.
    let code = allmystuff_cec_protocol::support_id_from_string(&format!("{tech}:{session_id}"));
    code.chars().take(6).collect()
}

/// Grouped display of a support number, e.g. `123 456 789` (cosmetic; the node
/// derives rooms from the normalized form).
pub fn grouped_number(number: &str) -> String {
    format_support_id(number)
}

/// Build the daemon config for the **standing support area** — the one
/// well-known mesh every CEC node lives on
/// ([`HELP_NETWORK_ID`](allmystuff_cec_protocol::HELP_NETWORK_ID)):
/// `cecsupport-clients`, used by announcing customers and listen-only
/// technician watchers. It carries discovery only; actual support transports
/// live in [`session_network_config`] rooms.
///
/// **Silent.** MyOwnMesh is a signaling system for direct WebRTC
/// peer-to-peer connections, and the area uses exactly that: residents are
/// visible in the signaling room (a technician's pinned redial finds a
/// rebooted customer; a phoned-in number resolves against the member list)
/// and mesh admission is disabled — no auto-connect, no roster gossip, and no
/// unilateral stale pin can create a data link. A deliberate dial happens only
/// after both sides meet in the customer's session room. That session is a
/// direct WebRTC link, with the venue/default TURN server as WebRTC's own NAT
/// fallback; human access remains gated by CEC consent per privileged frame.
///
pub fn help_network_config() -> (String, Value) {
    let network_id = allmystuff_cec_protocol::HELP_NETWORK_ID.to_string();
    let config = json!({
        "id": network_id,
        "network_id": network_id,
        "label": "CEC Support",
        "kind": "silent",
        // This room is a directory, never a data network. Even a stale or
        // hostile one-sided dial must not become an admitted mesh link.
        "auto_approve": false,
        "signaling": { "strategy": "nostr", "mdns": true },
    });
    (network_id, config)
}

/// Technician view of the public customer directory. It receives customer
/// announces but never publishes the technician's own presence.
pub fn help_watch_network_config() -> (String, Value) {
    let (network_id, mut config) = help_network_config();
    config["signaling"] = json!({
        "strategy": "nostr",
        "mdns": false,
        "listen_only": true,
    });
    (network_id, config)
}

/// Build the customer-specific Silent room that carries an actual support
/// session. Only the customer and a technician who selected that customer's
/// Support ID join it; the global area above remains signaling-only.
pub fn session_network_config(number: &str) -> (String, Value) {
    let network_id = allmystuff_cec_protocol::network_id_for_number(number);
    let config = json!({
        "id": network_id,
        "network_id": network_id,
        "label": format!("CEC Support {}", grouped_number(number)),
        "kind": "silent",
        // The technician's deliberate dial opens the transport. Human access
        // is still gated by the CEC consent request on every privileged path.
        "auto_approve": true,
        "signaling": { "strategy": "nostr", "mdns": true },
    });
    (network_id, config)
}

/// The bare digits of a support number — the tolerant-input form of the
/// dial-by-number fallback (the customer reads their digits over the phone;
/// the technician types them with or without spacing). Matching happens
/// against numbers derived from device ids in the public directory. The same
/// digits also derive the private session-room id.
pub fn number_digits(number: &str) -> String {
    number.chars().filter(|c| c.is_ascii_digit()).collect()
}

/// Canonicalise a device id to its bare pubkey — the same `-XXXXX` display
/// suffix strip the consent store and the mesh use, so a technician isn't seen
/// as a new peer across a reconnect. Re-exported shape of
/// [`allmystuff_cec_consent::pubkey_part`].
pub fn pubkey_part(id: &str) -> &str {
    allmystuff_cec_consent::pubkey_part(id)
}

/// Whether `network` is one of the CEC rooms — the world-joinable standing
/// support area or the asking room. The rooms where AllMyStuff's own
/// presence/graph protocol is gated to peers with a deliberate CEC
/// relationship ([`Cec::relationship_with`]), because "shares a room with
/// this node" means nothing there.
pub fn is_cec_network(network: &str) -> bool {
    network == allmystuff_cec_protocol::HELP_NETWORK_ID
        || network == allmystuff_cec_protocol::ASK_NETWORK_ID
        || network
            .strip_prefix(allmystuff_cec_protocol::CEC_NETWORK_PREFIX)
            .is_some_and(|tail| {
                tail.len() == allmystuff_cec_protocol::SUPPORT_ID_LEN
                    && tail.chars().all(|c| c.is_ascii_digit())
            })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn support_directory_and_session_have_separate_admission() {
        // The standing area is a Silent discovery-only directory with mesh
        // admission disabled and no topology to shape.
        let (id, cfg) = help_network_config();
        assert_eq!(id, allmystuff_cec_protocol::HELP_NETWORK_ID);
        assert_eq!(cfg["kind"], "silent");
        assert_eq!(cfg["auto_approve"], false);
        assert!(cfg.get("topology").is_none(), "signaling-only: no topology");
        assert_eq!(cfg["signaling"]["strategy"], "nostr");

        let (session_id, session) = session_network_config("123 456 789");
        assert_eq!(session_id, "cec-123456789");
        assert_eq!(session["kind"], "silent");
        assert_eq!(session["auto_approve"], true);
        assert!(is_cec_network(&session_id));
        assert!(!is_cec_network("cec-kvm-something"));

        let (_, watcher) = help_watch_network_config();
        assert_eq!(watcher["signaling"]["listen_only"], true);
        assert_eq!(watcher["signaling"]["mdns"], false);
    }

    const ME: &str = "customerpubkeybase32aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const TECH: &str = "techpubkeybase32bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn status_derives_number_and_reports_the_public_directory() {
        let cec = Cec::new(None);
        let v = cec.status(Some(ME));
        assert_eq!(v["number"], support_id_from_device(ME));
        // Existing clients receive the public-directory id in status; session
        // room selection happens internally when a technician dials.
        assert_eq!(v["network_id"], allmystuff_cec_protocol::HELP_NETWORK_ID);
        assert_eq!(v["role"], "client");
    }

    #[test]
    fn approve_then_gate_allows_control_and_revoke_bites() {
        let cec = Cec::new(None);
        cec.record_pending(PendingRequest {
            tech: TECH.into(),
            agent_name: "Alex at CEC".into(),
            want_control: true,
            session_id: "s1".into(),
            verification_code: verification_code(TECH, "s1"),
        });
        assert_eq!(cec.pending().len(), 1);
        assert!(!cec.is_allowed(TECH, Capability::Control));

        cec.approve(TECH, "Alex at CEC", ApprovalScope::Once, true)
            .unwrap();
        // The grant now gates control, and the pending request cleared.
        assert!(cec.is_allowed(TECH, Capability::Control));
        assert!(cec.is_allowed(TECH, Capability::ScreenView));
        assert!(cec.pending().is_empty());
        assert_eq!(cec.grants().len(), 1);

        // "Forget this technician" — the gate closes immediately.
        assert!(cec.revoke(TECH).unwrap());
        assert!(!cec.is_allowed(TECH, Capability::Control));
        assert!(cec.grants().is_empty());
    }

    #[test]
    fn view_only_grant_does_not_authorise_control() {
        let cec = Cec::new(None);
        cec.approve(TECH, "Alex", ApprovalScope::Forever, false)
            .unwrap();
        assert!(cec.is_allowed(TECH, Capability::ScreenView));
        assert!(!cec.is_allowed(TECH, Capability::Control));
    }

    // ---- session ↔ technician binding (the consent sweep's teardown map) ----

    const TECH_B: &str = "techpubkeybase32cccccccccccccccccccccccccccccccccccc";

    #[test]
    fn terminal_dial_transition_cannot_overwrite_an_answer() {
        let cec = Cec::new(None);
        cec.set_session("waiting", "requested");
        assert!(cec.transition_session("waiting", "requested", "ended"));
        assert_eq!(cec.session_state("waiting").as_deref(), Some("ended"));

        cec.set_session("answered", "requested");
        cec.set_session("answered", "active");
        assert!(!cec.transition_session("answered", "requested", "ended"));
        assert_eq!(cec.session_state("answered").as_deref(), Some("active"));
    }

    #[test]
    fn end_sessions_for_returns_only_that_techs_sessions_and_clears_them() {
        let cec = Cec::new(None);
        // Two live sessions for TECH, one for a different technician.
        cec.set_session("s1", "active");
        cec.bind_session("s1", TECH);
        cec.set_session("s2", "active");
        cec.bind_session("s2", TECH);
        cec.set_session("s3", "active");
        cec.bind_session("s3", TECH_B);

        let mut ended = cec.end_sessions_for(TECH);
        ended.sort();
        assert_eq!(ended, vec!["s1".to_string(), "s2".to_string()]);
        // TECH's sessions are gone from the live-session map…
        assert_eq!(cec.session_state("s1"), None);
        assert_eq!(cec.session_state("s2"), None);
        // …while the other technician's session is untouched.
        assert_eq!(cec.session_state("s3").as_deref(), Some("active"));
        // A second call finds nothing left to end.
        assert!(cec.end_sessions_for(TECH).is_empty());
    }

    #[test]
    fn session_binding_canonicalises_the_technician_id() {
        let cec = Cec::new(None);
        // Bound under a display-suffixed id, ended by the bare pubkey (and
        // vice-versa) — the sweep must reach it however the id arrives.
        cec.set_session("s1", "active");
        cec.bind_session("s1", &format!("{TECH}-AB12C"));
        let ended = cec.end_sessions_for(TECH);
        assert_eq!(ended, vec!["s1".to_string()]);
    }

    #[test]
    fn terminal_state_drops_the_session_binding() {
        let cec = Cec::new(None);
        cec.set_session("s1", "active");
        cec.bind_session("s1", TECH);
        // A normal end (or deny) clears the binding, so a later lapse-driven
        // sweep can't resurrect a stale "ended" for a reused technician.
        cec.set_session("s1", "ended");
        assert!(cec.end_sessions_for(TECH).is_empty());
    }

    #[test]
    fn support_kvm_requires_active_customer_session_and_short_lease() {
        let cec = Cec::new(None);
        cec.set_session("s1", "active");
        cec.bind_session("s1", TECH);
        assert!(cec.has_active_session_with(&format!("{TECH}-AB12C")));

        assert!(cec.note_support_kvm(TECH, "kvm-pub", 8));
        assert!(cec.relationship_with("kvm-pub"));
        assert_eq!(cec.support_kvm_customer("kvm-pub").as_deref(), Some(TECH));
        assert!(!cec.direct_relationship_with("kvm-pub"));
        assert!(!cec.note_support_kvm(TECH, "forever-kvm", 3_600));
        assert!(!cec.relationship_with("forever-kvm"));

        cec.set_session("s1", "ended");
        assert!(!cec.has_active_session_with(TECH));
    }

    #[test]
    fn active_session_technicians_are_canonical_deduped_and_live_only() {
        let cec = Cec::new(None);
        cec.set_session("active-a", "active");
        cec.bind_session("active-a", &format!("{TECH}-AB12C"));
        cec.set_session("active-b", "active");
        cec.bind_session("active-b", TECH);
        cec.set_session("waiting", "requested");
        cec.bind_session("waiting", TECH_B);

        // Local approval is not enough: the technician still rejects an
        // attached-KVM announcement until it has consumed Approve.
        assert!(cec.active_session_technicians().is_empty());
        assert!(cec.mark_kvm_support_ready("active-a"));
        assert!(cec.mark_kvm_support_ready("active-b"));
        assert!(!cec.mark_kvm_support_ready("waiting"));
        assert_eq!(cec.active_session_technicians(), vec![TECH.to_string()]);

        cec.set_session("active-a", "ended");
        cec.set_session("active-b", "ended");
        assert!(cec.active_session_technicians().is_empty());

        // A reused id cannot inherit an old delivery acknowledgement.
        cec.set_session("active-a", "active");
        cec.bind_session("active-a", TECH);
        assert!(cec.active_session_technicians().is_empty());
    }

    #[test]
    fn dialed_directory_is_device_keyed() {
        let cec = Cec::new(None);
        let canon = pubkey_part(TECH);
        // A dial records the row immediately (online=false pre-connect)…
        let row = cec.record_dialed(
            TECH.into(),
            "123456789".into(),
            String::new(),
            String::new(),
            false,
        );
        assert_eq!(row.node, TECH);
        assert_eq!(cec.dialed_list().len(), 1);
        // …and the post-connect refresh merges into the SAME row (same
        // device), filling ident and flipping online.
        cec.record_dialed(
            TECH.into(),
            "123456789".into(),
            "Reception PC".into(),
            "RECEPTION-01".into(),
            true,
        );
        assert_eq!(cec.dialed_list().len(), 1, "one row per device");
        assert!(cec.is_dialed(canon));
        // The dial stamps a `last_used` the CEC tab renders as time-since — it's
        // present, non-zero, and a `touch` refreshes the record for a re-emit.
        let listed = cec.dialed_list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["label"], "Reception PC");
        assert!(listed[0]["last_used"].as_u64().unwrap_or(0) > 0);
        assert!(cec.touch_dialed(canon).is_some());
        assert!(cec.touch_dialed("someone-we-never-dialed").is_none());
        assert!(cec.forget_dialed(canon));
        assert!(!cec.is_dialed(canon));
    }

    #[test]
    fn legacy_number_keyed_directory_migrates_by_device() {
        // An older directory: one completed dial (has a node), one nodeless
        // attempt row, both with an obsolete stored `network_id` field. The
        // completed row survives keyed by device; the nodeless attempt cannot
        // identify a customer and is dropped; the room field is ignored.
        let path =
            std::env::temp_dir().join(format!("cec-dialed-migration-{}.json", std::process::id()));
        std::fs::write(
            &path,
            serde_json::json!([
                {
                    "node": TECH,
                    "number": "123456789",
                    "label": "Reception PC",
                    "online": true,
                    "network_id": "cec-123456789",
                    "last_used": 1700000000
                },
                {
                    "node": "",
                    "number": "987654321",
                    "label": "",
                    "online": false,
                    "network_id": "cec-987654321",
                    "last_used": 1700000001
                }
            ])
            .to_string(),
        )
        .expect("write legacy directory");
        let loaded = super::load_dialed(Some(&path));
        let _ = std::fs::remove_file(&path);
        assert_eq!(loaded.len(), 1, "nodeless attempt rows are dropped");
        let row = loaded.get(pubkey_part(TECH)).expect("keyed by device");
        assert_eq!(row.number, "123456789");
        assert!(!row.online, "online is never trusted from disk");
    }

    #[test]
    fn parse_scope_round_trips() {
        assert_eq!(parse_scope("once").unwrap(), ApprovalScope::Once);
        assert_eq!(
            parse_scope("three_hours").unwrap(),
            ApprovalScope::ThreeHours
        );
        assert_eq!(parse_scope("forever").unwrap(), ApprovalScope::Forever);
        assert!(parse_scope("someday").is_err());
    }

    #[test]
    fn verification_code_is_stable_and_short() {
        let a = verification_code(TECH, "s1");
        let b = verification_code(TECH, "s1");
        assert_eq!(a, b);
        assert_eq!(a.len(), 6);
        assert_ne!(a, verification_code(TECH, "s2"));
    }
}
