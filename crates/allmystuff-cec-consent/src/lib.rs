//! # cec-support-consent
//!
//! A customer's standing decisions about **which technicians may connect, and
//! for how long**. This is the enforcement side of the three-choice prompt the
//! CEC Support app shows when a technician dials in:
//!
//! | Choice                      | [`ApprovalScope`]           | Stored where | Lifetime |
//! |-----------------------------|-----------------------------|--------------|----------|
//! | Approve Once                | [`Once`](ApprovalScope::Once) | memory only | this session |
//! | Auto-Approve for 3 hours    | [`ThreeHours`](ApprovalScope::ThreeHours) | disk | 3 hours |
//! | Auto-Approve Forever        | [`Forever`](ApprovalScope::Forever) | disk | until revoked |
//!
//! The store is the single source of truth the node consults **on every
//! privileged frame** (a technician screen-view or an input event), so a
//! revoke — the "Forget this technician" action — bites immediately, mid-session,
//! even if the wire "you're revoked" message is lost. That mirrors AllMyStuff's
//! rule that authorization is re-checked per frame, never cached for a session.
//!
//! ## Three questions, asked at three different moments
//!
//! [`ApprovalScope`] answers *how long*, per technician, at the connect prompt.
//! [`Capability`] answers *how far* — a three-rung ladder (see-the-screen →
//! drive-it → drive-it-as-Administrator) where each rung implies the ones below
//! and nothing implies a rung above.
//!
//! The top rung has its own clock. [`ElevationPolicy`] is a **machine-wide**
//! decision made **once, with the install**, not a question re-asked per
//! session: a customer whose PC is broken should not be adjudicating a
//! consent dialog at the start of every repair, because a prompt shown that
//! often stops being read. So administrator access is a setting — decided
//! while a technician is on the phone explaining the install, shown in
//! Settings afterwards, and switchable off there.
//!
//! Being a setting does not make it weaker than a prompt. It is re-read on
//! every privileged frame exactly like a grant is, so switching it off drops
//! admin reach mid-repair for every technician at once. What changed is *when
//! the customer is asked*, not *how well the answer is enforced*.
//!
//! ## Time is injected, never read
//!
//! Every method that cares about expiry takes `now` (unix seconds) as an
//! argument. The store never calls the clock itself, so the whole thing is
//! deterministic and unit-testable without sleeping. The daemon passes
//! `SystemTime::now()`.

mod persist;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use allmystuff_cec_protocol::ApprovalScope;

/// What a technician is allowed to do once approved.
///
/// These form a **ladder**: each rung implies every rung below it (see
/// [`Capability::covers`]). Nothing implies a rung *above* it — in particular
/// an ordinary `Control` grant never confers [`Elevated`], because "drive the
/// mouse" and "drive the machine as Administrator" are different decisions and
/// the customer must make the second one deliberately.
///
/// [`Elevated`]: Capability::Elevated
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// See the customer's screen.
    ScreenView,
    /// Drive the customer's keyboard and mouse (implies [`ScreenView`]).
    ///
    /// [`ScreenView`]: Capability::ScreenView
    Control,
    /// Drive the customer's machine **with administrator reach** (implies
    /// [`Control`]): input that lands in elevated windows (Event Viewer,
    /// Services, Device Manager, regedit, an admin PowerShell) and a screen
    /// stream that keeps painting across Windows' *secure desktop* — the
    /// separate desktop UAC's consent dialog runs on.
    ///
    /// Without this, Windows itself stops a support session dead: UIPI (User
    /// Interface Privilege Isolation) silently discards synthesized input aimed
    /// at any window running at a higher integrity level than the sender, and
    /// the secure desktop is a different desktop object that an ordinary
    /// medium-integrity process can neither capture nor type into. The
    /// technician sees the repair tool and cannot touch it.
    ///
    /// It is a real escalation, so it takes two independent yeses: the
    /// technician asks for it (`want_elevated`, recorded on the grant) **and**
    /// the machine permits it ([`ElevationPolicy`], the customer's one-time
    /// install decision). Both are re-read on every privileged frame, so it
    /// dies the instant either the grant is revoked or the setting is switched
    /// off.
    ///
    /// [`Control`]: Capability::Control
    Elevated,
}

impl Capability {
    /// Position on the capability ladder. Higher covers lower.
    fn rank(self) -> u8 {
        match self {
            Capability::ScreenView => 0,
            Capability::Control => 1,
            Capability::Elevated => 2,
        }
    }

    /// Whether holding `self` satisfies a request for `wanted`. `Elevated`
    /// implies `Control` implies `ScreenView`; never the other way round.
    fn covers(self, wanted: Capability) -> bool {
        self.rank() >= wanted.rank()
    }
}

/// The safe minimum a grant carries when its `capabilities` field is absent
/// (an older persisted grant): view-only, never control, never elevated. A
/// missing field must never widen access.
fn default_capabilities() -> Vec<Capability> {
    vec![Capability::ScreenView]
}

/// Map the wire `want_control` flag to the capability set a grant should carry.
/// Shorthand for [`capabilities_for_request`] with no elevation asked for — the
/// shape every caller that predates admin access still wants.
pub fn capabilities_for(want_control: bool) -> Vec<Capability> {
    capabilities_for_request(want_control, false)
}

/// Map the wire request flags to the capability set a grant should carry.
///
/// `want_elevated` records that the *technician asked* for administrator reach.
/// It is only half the answer — [`ConsentStore::is_allowed`] also requires the
/// machine's [`ElevationPolicy`] to be on — so recording it here never widens
/// anything on its own, and a view-only technician still can't reach admin
/// even on a machine that permits it.
///
/// `want_elevated` implies control whether or not the technician also set
/// `want_control`: administrator reach that couldn't move the mouse would be a
/// grant with nothing to drive, and honouring the pair separately would let a
/// malformed request record "elevated but view-only", a state no gate below
/// knows how to read. Normalizing here keeps the stored ladder consistent.
pub fn capabilities_for_request(want_control: bool, want_elevated: bool) -> Vec<Capability> {
    if want_elevated {
        vec![
            Capability::ScreenView,
            Capability::Control,
            Capability::Elevated,
        ]
    } else if want_control {
        vec![Capability::ScreenView, Capability::Control]
    } else {
        vec![Capability::ScreenView]
    }
}

/// One standing approval of one technician.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    /// The technician's canonical device id (base32 pubkey, display suffix
    /// stripped — see [`pubkey_part`]).
    pub technician: String,
    /// The Agent Name the customer saw when approving ("*so-and-so* is trying
    /// to connect"). Kept so the customer can recognise the entry when they
    /// choose to forget it.
    #[serde(default)]
    pub agent_name: String,
    /// What the technician may do. Defaults to view-only ([`Capability::ScreenView`])
    /// when a persisted grant predates this field, so an older store still loads
    /// (at minimum privilege) instead of being discarded.
    #[serde(default = "default_capabilities")]
    pub capabilities: Vec<Capability>,
    /// Why this grant exists / how it was made.
    pub scope: ApprovalScope,
    /// Unix seconds the grant was made.
    pub granted_at: u64,
    /// Absolute expiry (unix seconds), or `None` for [`ApprovalScope::Forever`].
    #[serde(default)]
    pub expires_at: Option<u64>,
}

impl Grant {
    fn is_live(&self, now: u64) -> bool {
        match self.expires_at {
            Some(deadline) => now < deadline,
            None => true,
        }
    }

    fn covers(&self, cap: Capability) -> bool {
        self.capabilities.iter().any(|held| held.covers(cap))
    }

    /// Whether this grant confers `cap`, ignoring expiry. For *display* — the
    /// customer's access list showing a "Control" or "Admin access" badge —
    /// never for enforcement, which goes through [`ConsentStore::is_allowed`]
    /// so the clock is always consulted.
    pub fn allows(&self, cap: Capability) -> bool {
        self.covers(cap)
    }
}

/// Errors from a durable consent operation.
#[derive(Debug, Error)]
pub enum ConsentError {
    /// A persistent grant could not be written to disk. The caller must treat
    /// this as a **failed** approval and not proceed — an unsaved "Auto-Approve
    /// Forever" that silently reverts to prompting on the next boot is a
    /// security downgrade, so the store never acknowledges state it couldn't
    /// save.
    #[error("could not save consent store: {0}")]
    Persist(#[from] std::io::Error),
}

/// The machine-wide answer to "may CEC support sessions use administrator
/// access on this PC?" — asked **once**, when the background service is
/// installed, and never again.
///
/// Admin reach is deliberately not a per-session question. The customer is
/// someone whose PC is broken; making them adjudicate a UAC-shaped prompt at
/// the start of every repair trains them to click through it, which is worse
/// for them than one considered decision made at install time while a
/// technician is explaining it. So the elevation rung is a *setting*, decided
/// with the install (which already costs one UAC prompt to register a service),
/// visible in Settings, and revocable there at any time.
///
/// It is still a live gate, not just an install-time input: [`ConsentStore::is_allowed`]
/// consults it on every privileged frame, so switching it off in Settings drops
/// administrator reach mid-session for every technician at once — the machine-wide
/// twin of "Forget this technician".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ElevationPolicy {
    /// Whether approved technicians may use administrator access here.
    pub allowed: bool,
    /// Unix seconds the customer decided, for the Settings line ("allowed since
    /// 3 March"). Display only.
    #[serde(default)]
    pub decided_at: u64,
}

/// On-disk shape. Only persistent grants (`ThreeHours`, `Forever`) are written;
/// `Once` grants live in [`ConsentStore::ephemeral`] and never touch disk.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Persisted {
    #[serde(default)]
    version: u32,
    /// The machine-wide elevation decision, absent until the customer makes it.
    /// Absent means **not allowed** — an undecided machine is a locked-down one.
    #[serde(default)]
    elevation: Option<ElevationPolicy>,
    /// Loaded **per-grant tolerantly** (see [`de_lenient_grants`]): an unreadable
    /// grant is dropped on its own instead of failing the whole load. Previously
    /// one grant a reader build couldn't parse (a scope/shape skew from a
    /// differently-versioned node sharing the same consent file) discarded
    /// EVERY standing approval, so a customer had to re-approve after a restart.
    #[serde(default, deserialize_with = "de_lenient_grants")]
    grants: Vec<Grant>,
}

/// Deserialize the grant list element-by-element, keeping the readable grants
/// and silently dropping any that don't parse — so one malformed or
/// older-shaped grant can never erase a customer's whole standing consent. The
/// per-field tolerance on [`Grant`] (defaulted `capabilities`, string-or-tagged
/// `scope`) means legacy grants are *read*, not dropped; this is the backstop
/// for anything still unreadable.
fn de_lenient_grants<'de, D>(deserializer: D) -> Result<Vec<Grant>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Vec::<serde_json::Value>::deserialize(deserializer)?;
    Ok(raw
        .into_iter()
        .filter_map(|v| serde_json::from_value::<Grant>(v).ok())
        .collect())
}

const STORE_VERSION: u32 = 1;

/// A customer's approvals. Load with [`ConsentStore::load`]; approve/revoke as
/// the customer taps the prompt; check with [`ConsentStore::is_allowed`] on
/// every privileged frame.
#[derive(Debug, Default)]
pub struct ConsentStore {
    /// `None` for an in-memory-only store (tests, or a run with no home dir).
    path: Option<PathBuf>,
    /// Persistent grants (`ThreeHours` + `Forever`), mirrored to `path`.
    persistent: Vec<Grant>,
    /// `Once` grants for the current run only. Never serialised.
    ephemeral: Vec<Grant>,
    /// The machine-wide admin-access decision, `None` until the customer makes
    /// it. Checked live by [`ConsentStore::is_allowed`], not just at approve time.
    elevation: Option<ElevationPolicy>,
}

impl ConsentStore {
    /// Load the store from `path`. A missing file yields an empty store; a
    /// corrupt file is quarantined aside (`<path>.corrupt`) and the store
    /// starts empty rather than bricking the app — the same tolerant-load
    /// discipline AllMyStuff uses. Does **not** prune expired grants; queries
    /// filter by `now` at read time.
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let loaded: Persisted = persist::load_json(&path);
        ConsentStore {
            path: Some(path),
            persistent: loaded.grants,
            ephemeral: Vec::new(),
            elevation: loaded.elevation,
        }
    }

    /// An in-memory store with no disk backing. Persistent grants are kept in
    /// memory but never written (as if the machine had no home dir).
    pub fn in_memory() -> Self {
        ConsentStore::default()
    }

    /// Record the customer's choice for `technician`. Replaces any existing
    /// grant for the same technician (canonicalised by [`pubkey_part`]).
    ///
    /// - [`ApprovalScope::Once`] is stored in memory only.
    /// - [`ApprovalScope::ThreeHours`] / [`ApprovalScope::Forever`] are written
    ///   to disk; a failed write returns [`ConsentError::Persist`] and the grant
    ///   is **not** recorded.
    pub fn approve(
        &mut self,
        technician: &str,
        agent_name: &str,
        capabilities: Vec<Capability>,
        scope: ApprovalScope,
        now: u64,
    ) -> Result<(), ConsentError> {
        let key = pubkey_part(technician).to_string();
        let grant = Grant {
            technician: key.clone(),
            agent_name: agent_name.to_string(),
            capabilities,
            scope,
            granted_at: now,
            expires_at: scope.expires_at(now),
        };

        // A technician can only have one live grant; a new decision replaces
        // any prior one in either tier.
        self.ephemeral.retain(|g| g.technician != key);
        self.persistent.retain(|g| g.technician != key);

        if scope.persists() {
            self.persistent.push(grant);
            self.save()?; // roll back in memory if the durable write fails
        } else {
            self.ephemeral.push(grant);
        }
        Ok(())
    }

    /// Whether `technician` currently holds a live grant covering `cap`. This
    /// is the per-frame enforcement check; it consults both the in-memory
    /// `Once` grants and the persisted ones, filtered by `now`.
    ///
    /// [`Capability::Elevated`] carries a second, machine-wide condition: the
    /// customer's install-time admin-access decision must also still be on.
    /// Checking it *here* rather than only when the grant was made is what makes
    /// the Settings switch a real kill switch — turning it off drops
    /// administrator reach on the next frame for every technician at once,
    /// without having to walk and rewrite the stored grants.
    pub fn is_allowed(&self, technician: &str, cap: Capability, now: u64) -> bool {
        if cap == Capability::Elevated && !self.elevation_allowed() {
            return false;
        }
        let key = pubkey_part(technician);
        self.persistent
            .iter()
            .chain(self.ephemeral.iter())
            .any(|g| g.technician == key && g.is_live(now) && g.covers(cap))
    }

    /// The machine-wide admin-access decision, or `None` if the customer has
    /// never been asked (a fresh install that hasn't run the service installer).
    /// The caller uses `None` to know it should *ask* — once.
    pub fn elevation_policy(&self) -> Option<ElevationPolicy> {
        self.elevation
    }

    /// Whether administrator access is permitted on this machine at all.
    /// Undecided reads as **not allowed**: absence must never widen reach.
    pub fn elevation_allowed(&self) -> bool {
        matches!(self.elevation, Some(p) if p.allowed)
    }

    /// Record the customer's machine-wide admin-access decision — the one made
    /// with the install, and afterwards only from Settings.
    ///
    /// Persisted like a durable grant: a failed write returns the error and
    /// changes nothing, so a machine can never believe it allows administrator
    /// access that its disk doesn't record.
    pub fn set_elevation_policy(&mut self, allowed: bool, now: u64) -> Result<(), ConsentError> {
        let previous = self.elevation;
        self.elevation = Some(ElevationPolicy {
            allowed,
            decided_at: now,
        });
        if let Err(e) = self.save() {
            self.elevation = previous; // roll back; never acknowledge unsaved state
            return Err(e);
        }
        Ok(())
    }

    /// Whether `technician` has **any** grant record here, live or lapsed.
    /// This is recognition, not authorization — [`Self::is_allowed`] stays the
    /// only enforcement check. It lets the node keep treating a peer as a CEC
    /// technician *after* their grant expires: the screen-offer screen must
    /// still apply to them (an expired technician is not a stranger the CEC
    /// gate can ignore), and a refusal can name the real cause ("approval
    /// lapsed") instead of pointing at fleet settings.
    pub fn known(&self, technician: &str) -> bool {
        let key = pubkey_part(technician);
        self.persistent
            .iter()
            .chain(self.ephemeral.iter())
            .any(|g| g.technician == key)
    }

    /// Revoke every grant for `technician` — the "Forget this technician"
    /// action. Removes both the persisted and the in-memory grant and persists
    /// the change. Returns `true` if anything was actually removed.
    pub fn revoke(&mut self, technician: &str) -> Result<bool, ConsentError> {
        let key = pubkey_part(technician).to_string();
        let before = self.persistent.len() + self.ephemeral.len();
        self.ephemeral.retain(|g| g.technician != key);
        let had_persistent = self.persistent.iter().any(|g| g.technician == key);
        self.persistent.retain(|g| g.technician != key);
        if had_persistent {
            self.save()?;
        }
        Ok(before != self.persistent.len() + self.ephemeral.len())
    }

    /// Drop the caller's in-memory `Once` grants — call at session end so an
    /// "Approve Once" never outlives the session it was for.
    pub fn clear_once(&mut self) {
        self.ephemeral.clear();
    }

    /// Remove only the in-memory `Once` grant for `technician` — the session
    /// it covered just ended. Persistent grants are untouched (a
    /// 3-hours/Forever choice deliberately outlives sessions). Returns whether
    /// anything was removed.
    pub fn revoke_once(&mut self, technician: &str) -> bool {
        let key = pubkey_part(technician).to_string();
        let before = self.ephemeral.len();
        self.ephemeral.retain(|g| g.technician != key);
        before != self.ephemeral.len()
    }

    /// Remove any expired persistent grants and persist if anything changed.
    /// Returns how many were pruned. Safe to call on a schedule.
    pub fn purge_expired(&mut self, now: u64) -> Result<usize, ConsentError> {
        let before = self.persistent.len();
        self.persistent.retain(|g| g.is_live(now));
        let pruned = before - self.persistent.len();
        if pruned > 0 {
            self.save()?;
        }
        Ok(pruned)
    }

    /// The live grants a customer would see in a "who can reach me" list
    /// (persistent + in-memory, expired ones filtered out). Sorted by most
    /// recent first.
    pub fn active_grants(&self, now: u64) -> Vec<Grant> {
        let mut out: Vec<Grant> = self
            .persistent
            .iter()
            .chain(self.ephemeral.iter())
            .filter(|g| g.is_live(now))
            .cloned()
            .collect();
        out.sort_by_key(|g| std::cmp::Reverse(g.granted_at));
        out
    }

    /// The file this store persists to, if any.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    fn save(&self) -> Result<(), ConsentError> {
        let Some(path) = &self.path else {
            return Ok(()); // in-memory store: nothing to write
        };
        let doc = Persisted {
            version: STORE_VERSION,
            elevation: self.elevation,
            grants: self.persistent.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&doc).expect("consent store serialises");
        persist::write_atomic(path, &bytes)?;
        Ok(())
    }
}

/// Strip a trailing `-XXXXX` display suffix (dash + 5 alphanumerics) from a
/// device id, returning the canonical bare pubkey. A technician id arrives in
/// display form (`pubkey-AB12C`) or bare form depending on the surface, and
/// every store operation canonicalises through this so a reconnecting
/// technician isn't seen as a new, ungranted peer. Matches MyOwnMesh's
/// `signing::pubkey_part`.
pub fn pubkey_part(device_id: &str) -> &str {
    if let Some((head, tail)) = device_id.rsplit_once('-') {
        if tail.len() == 5 && tail.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return head;
        }
    }
    device_id
}

#[cfg(test)]
mod tests {
    use super::*;
    use allmystuff_cec_protocol::THREE_HOURS_SECS;

    const T0: u64 = 1_700_000_000;
    const TECH: &str = "techpubkeybase32aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn tempstore() -> (tempfile::TempDir, ConsentStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = ConsentStore::load(dir.path().join("consent.json"));
        (dir, store)
    }

    #[test]
    fn once_is_not_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("consent.json");
        {
            let mut s = ConsentStore::load(&path);
            s.approve(
                TECH,
                "Alex",
                capabilities_for(true),
                ApprovalScope::Once,
                T0,
            )
            .unwrap();
            assert!(s.is_allowed(TECH, Capability::Control, T0));
        }
        // Reload: the Once grant is gone.
        let reloaded = ConsentStore::load(&path);
        assert!(!reloaded.is_allowed(TECH, Capability::ScreenView, T0));
    }

    #[test]
    fn known_recognises_lapsed_grants_but_never_authorises() {
        let (_dir, mut s) = tempstore();
        assert!(!s.known(TECH), "a stranger is not known");
        s.approve(
            TECH,
            "Alex",
            capabilities_for(true),
            ApprovalScope::ThreeHours,
            T0,
        )
        .unwrap();
        // Live: known and allowed. Lapsed: still known — the CEC gates must
        // keep screening an expired technician — but no longer allowed.
        let display = format!("{TECH}-AB12C");
        assert!(s.known(&display), "recognition canonicalises the id");
        assert!(s.is_allowed(TECH, Capability::Control, T0 + 10));
        let lapsed = T0 + THREE_HOURS_SECS + 1;
        assert!(s.known(TECH), "expiry does not erase recognition");
        assert!(!s.is_allowed(TECH, Capability::Control, lapsed));
        // A revoke ("Forget this technician") erases recognition too.
        assert!(s.revoke(TECH).unwrap());
        assert!(!s.known(TECH));
    }

    #[test]
    fn forever_persists_across_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("consent.json");
        {
            let mut s = ConsentStore::load(&path);
            s.approve(
                TECH,
                "Alex",
                capabilities_for(false),
                ApprovalScope::Forever,
                T0,
            )
            .unwrap();
        }
        let reloaded = ConsentStore::load(&path);
        assert!(reloaded.is_allowed(TECH, Capability::ScreenView, T0 + 999_999));
        // View-only grant does not authorise control.
        assert!(!reloaded.is_allowed(TECH, Capability::Control, T0));
    }

    #[test]
    fn three_hours_expires() {
        let (_dir, mut s) = tempstore();
        s.approve(
            TECH,
            "Alex",
            capabilities_for(true),
            ApprovalScope::ThreeHours,
            T0,
        )
        .unwrap();
        assert!(s.is_allowed(TECH, Capability::Control, T0 + 10));
        assert!(s.is_allowed(TECH, Capability::Control, T0 + THREE_HOURS_SECS - 1));
        // At and past the deadline, no longer allowed.
        assert!(!s.is_allowed(TECH, Capability::Control, T0 + THREE_HOURS_SECS));
        assert!(!s.is_allowed(TECH, Capability::Control, T0 + THREE_HOURS_SECS + 1));
    }

    #[test]
    fn revoke_bites_immediately() {
        let (_dir, mut s) = tempstore();
        s.approve(
            TECH,
            "Alex",
            capabilities_for(true),
            ApprovalScope::Forever,
            T0,
        )
        .unwrap();
        assert!(s.is_allowed(TECH, Capability::Control, T0));
        assert!(s.revoke(TECH).unwrap());
        assert!(!s.is_allowed(TECH, Capability::Control, T0));
        // Revoking again is a no-op that reports nothing removed.
        assert!(!s.revoke(TECH).unwrap());
    }

    #[test]
    fn revoke_removes_a_once_grant_too() {
        let (_dir, mut s) = tempstore();
        s.approve(
            TECH,
            "Alex",
            capabilities_for(false),
            ApprovalScope::Once,
            T0,
        )
        .unwrap();
        assert!(s.is_allowed(TECH, Capability::ScreenView, T0));
        assert!(s.revoke(TECH).unwrap());
        assert!(!s.is_allowed(TECH, Capability::ScreenView, T0));
    }

    #[test]
    fn approve_replaces_prior_decision() {
        let (_dir, mut s) = tempstore();
        // First a 3-hour control grant...
        s.approve(
            TECH,
            "Alex",
            capabilities_for(true),
            ApprovalScope::ThreeHours,
            T0,
        )
        .unwrap();
        // ...then the customer downgrades to Once view-only. The old one is gone.
        s.approve(
            TECH,
            "Alex",
            capabilities_for(false),
            ApprovalScope::Once,
            T0,
        )
        .unwrap();
        assert!(s.is_allowed(TECH, Capability::ScreenView, T0));
        assert!(!s.is_allowed(TECH, Capability::Control, T0));
        // And it did not survive as a persistent grant.
        assert_eq!(s.persistent.len(), 0);
        assert_eq!(s.ephemeral.len(), 1);
    }

    #[test]
    fn display_suffix_is_canonicalised() {
        let (_dir, mut s) = tempstore();
        // Approve by display id, check by bare pubkey and vice-versa.
        let display = format!("{TECH}-AB12C");
        s.approve(
            &display,
            "Alex",
            capabilities_for(true),
            ApprovalScope::Forever,
            T0,
        )
        .unwrap();
        assert!(s.is_allowed(TECH, Capability::Control, T0));
        assert!(s.is_allowed(&format!("{TECH}-ZZ99Q"), Capability::Control, T0));
        assert!(s.revoke(TECH).unwrap());
    }

    #[test]
    fn purge_expired_prunes_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("consent.json");
        let mut s = ConsentStore::load(&path);
        s.approve(
            TECH,
            "Alex",
            capabilities_for(true),
            ApprovalScope::ThreeHours,
            T0,
        )
        .unwrap();
        assert_eq!(s.purge_expired(T0 + 10).unwrap(), 0);
        assert_eq!(s.purge_expired(T0 + THREE_HOURS_SECS + 1).unwrap(), 1);
        // The prune was persisted.
        let reloaded = ConsentStore::load(&path);
        assert!(reloaded.active_grants(T0).is_empty());
    }

    #[test]
    fn corrupt_file_is_tolerated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("consent.json");
        std::fs::write(&path, b"{ this is not json").unwrap();
        let s = ConsentStore::load(&path);
        assert!(s.active_grants(T0).is_empty());
        // The bad file was quarantined, not left to break the next save.
        assert!(
            path.with_extension("json.corrupt").exists()
                || path.with_file_name("consent.json.corrupt").exists()
        );
    }

    #[test]
    fn pubkey_part_strips_only_a_real_suffix() {
        assert_eq!(pubkey_part("abc-AB12C"), "abc");
        assert_eq!(pubkey_part("abc-def"), "abc-def"); // tail not 5 chars
        assert_eq!(pubkey_part("abc"), "abc");
        assert_eq!(pubkey_part("abc-AB1!C"), "abc-AB1!C"); // non-alnum
    }

    // ---- shape migration + tolerant load (the "can't reuse the approval" bug) --

    #[test]
    fn legacy_bare_string_scope_and_missing_capabilities_still_load() {
        // A grant persisted by an older build: `scope` a bare string (not the
        // tagged {"kind":...} object) and no `capabilities` field at all. The
        // tolerant Grant deserializer must read it — view-only, the safe
        // minimum — instead of the whole store failing and re-prompting.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("consent.json");
        let legacy = format!(
            r#"{{ "version": 1, "grants": [
                {{ "technician": "{TECH}", "agent_name": "Alex",
                   "scope": "forever", "granted_at": {T0} }}
            ] }}"#
        );
        std::fs::write(&path, legacy).unwrap();
        let s = ConsentStore::load(&path);
        // The Forever grant survived the reload and still authorises view.
        assert!(s.is_allowed(TECH, Capability::ScreenView, T0 + 999_999));
        // Missing capabilities defaulted to view-only — never silently control.
        assert!(!s.is_allowed(TECH, Capability::Control, T0 + 999_999));
        // The file was NOT quarantined — it read cleanly.
        assert!(!path.with_file_name("consent.json.corrupt").exists());
    }

    #[test]
    fn tagged_scope_object_still_loads() {
        // The current on-disk shape (tagged scope object) must keep working.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("consent.json");
        let current = format!(
            r#"{{ "version": 1, "grants": [
                {{ "technician": "{TECH}", "agent_name": "Alex",
                   "capabilities": ["screen_view","control"],
                   "scope": {{ "kind": "forever" }}, "granted_at": {T0} }}
            ] }}"#
        );
        std::fs::write(&path, current).unwrap();
        let s = ConsentStore::load(&path);
        assert!(s.is_allowed(TECH, Capability::Control, T0 + 999_999));
    }

    #[test]
    fn one_unreadable_grant_does_not_wipe_the_readable_ones() {
        // The crux of the reuse bug: a single grant the reader can't parse must
        // drop on its own, NOT discard every standing approval in the file.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("consent.json");
        let good = "goodpubkeybase32aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let mixed = format!(
            r#"{{ "version": 1, "grants": [
                {{ "technician": "{good}", "agent_name": "Alex",
                   "capabilities": ["screen_view"], "scope": {{ "kind": "forever" }},
                   "granted_at": {T0} }},
                {{ "technician": "brokenrow", "scope": 12345 }}
            ] }}"#
        );
        std::fs::write(&path, mixed).unwrap();
        let s = ConsentStore::load(&path);
        // The good grant survived; only the broken one was dropped.
        assert!(s.is_allowed(good, Capability::ScreenView, T0 + 10));
        assert_eq!(s.active_grants(T0 + 10).len(), 1);
    }

    // ---- the elevation rung -------------------------------------------------

    /// A store whose machine-wide admin-access decision is already "yes" — the
    /// state a PC is in after the install-time question was answered.
    fn tempstore_admin_allowed() -> (tempfile::TempDir, ConsentStore) {
        let (dir, mut s) = tempstore();
        s.set_elevation_policy(true, T0).unwrap();
        (dir, s)
    }

    #[test]
    fn an_undecided_machine_allows_no_elevation() {
        // A fresh install has never been asked. Absence must read as "no".
        let (_dir, s) = tempstore();
        assert_eq!(s.elevation_policy(), None);
        assert!(!s.elevation_allowed());
    }

    #[test]
    fn the_install_time_decision_persists_and_is_not_re_asked() {
        // The whole point: answered once, still answered after a restart, so
        // nothing has to prompt the customer again.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("consent.json");
        {
            let mut s = ConsentStore::load(&path);
            s.set_elevation_policy(true, T0).unwrap();
        }
        let reloaded = ConsentStore::load(&path);
        assert!(reloaded.elevation_allowed());
        assert_eq!(reloaded.elevation_policy().unwrap().decided_at, T0);
    }

    #[test]
    fn a_session_gets_admin_with_no_second_prompt() {
        // The customer answered at install. A technician dials in asking for
        // admin reach and simply has it — no further consent step anywhere.
        let (_dir, mut s) = tempstore_admin_allowed();
        s.approve(
            TECH,
            "Alex",
            capabilities_for_request(true, true),
            ApprovalScope::Forever,
            T0,
        )
        .unwrap();
        assert!(s.is_allowed(TECH, Capability::Elevated, T0));
    }

    #[test]
    fn turning_the_setting_off_bites_existing_grants_immediately() {
        // The kill switch. An already-granted, already-running technician loses
        // admin reach on the next frame — without rewriting a single grant.
        let (_dir, mut s) = tempstore_admin_allowed();
        s.approve(
            TECH,
            "Alex",
            capabilities_for_request(true, true),
            ApprovalScope::Forever,
            T0,
        )
        .unwrap();
        assert!(s.is_allowed(TECH, Capability::Elevated, T0));

        s.set_elevation_policy(false, T0 + 5).unwrap();
        assert!(!s.is_allowed(TECH, Capability::Elevated, T0 + 6));
        // …and only the top rung goes. The session keeps working, unelevated.
        assert!(s.is_allowed(TECH, Capability::Control, T0 + 6));
        assert!(s.is_allowed(TECH, Capability::ScreenView, T0 + 6));
    }

    #[test]
    fn the_machine_setting_alone_does_not_elevate_anyone() {
        // Permitting admin on the machine is not granting it to a person: a
        // view-only technician stays view-only on a PC that allows admin.
        let (_dir, mut s) = tempstore_admin_allowed();
        s.approve(
            TECH,
            "Alex",
            capabilities_for(false),
            ApprovalScope::Forever,
            T0,
        )
        .unwrap();
        assert!(s.is_allowed(TECH, Capability::ScreenView, T0));
        assert!(!s.is_allowed(TECH, Capability::Control, T0));
        assert!(!s.is_allowed(TECH, Capability::Elevated, T0));
    }

    #[test]
    fn a_grant_asking_for_admin_on_a_disallowing_machine_gets_none() {
        // Both halves are required, and the machine half is the one that
        // decides. The grant records the ask; the policy withholds the reach.
        let (_dir, mut s) = tempstore(); // never decided → not allowed
        s.approve(
            TECH,
            "Alex",
            capabilities_for_request(true, true),
            ApprovalScope::Forever,
            T0,
        )
        .unwrap();
        assert!(!s.is_allowed(TECH, Capability::Elevated, T0));
        assert!(s.is_allowed(TECH, Capability::Control, T0));
    }

    #[test]
    fn control_never_confers_elevation() {
        // The whole point of the separate rung: approving "take control" must
        // NOT hand over administrator reach — even on a machine that permits
        // admin, which is what makes this assertion mean something.
        let (_dir, mut s) = tempstore_admin_allowed();
        s.approve(
            TECH,
            "Alex",
            capabilities_for(true),
            ApprovalScope::Forever,
            T0,
        )
        .unwrap();
        assert!(s.is_allowed(TECH, Capability::Control, T0));
        assert!(s.is_allowed(TECH, Capability::ScreenView, T0));
        assert!(!s.is_allowed(TECH, Capability::Elevated, T0));
    }

    #[test]
    fn elevated_covers_the_whole_ladder() {
        let (_dir, mut s) = tempstore_admin_allowed();
        s.approve(
            TECH,
            "Alex",
            capabilities_for_request(true, true),
            ApprovalScope::Forever,
            T0,
        )
        .unwrap();
        assert!(s.is_allowed(TECH, Capability::Elevated, T0));
        assert!(s.is_allowed(TECH, Capability::Control, T0));
        assert!(s.is_allowed(TECH, Capability::ScreenView, T0));
    }

    #[test]
    fn elevation_request_implies_control() {
        // A request for admin reach without control is normalized rather than
        // stored as an unreadable "elevated but view-only" grant.
        let caps = capabilities_for_request(false, true);
        assert!(caps.contains(&Capability::Control));
        assert!(caps.contains(&Capability::Elevated));
        assert!(caps.contains(&Capability::ScreenView));
    }

    #[test]
    fn revoke_drops_elevation_immediately() {
        // Admin reach must die on revoke exactly like control does — the
        // per-frame check is the whole enforcement story.
        let (_dir, mut s) = tempstore_admin_allowed();
        s.approve(
            TECH,
            "Alex",
            capabilities_for_request(true, true),
            ApprovalScope::Forever,
            T0,
        )
        .unwrap();
        assert!(s.is_allowed(TECH, Capability::Elevated, T0));
        assert!(s.revoke(TECH).unwrap());
        assert!(!s.is_allowed(TECH, Capability::Elevated, T0));
    }

    #[test]
    fn elevation_expires_with_its_grant() {
        let (_dir, mut s) = tempstore_admin_allowed();
        s.approve(
            TECH,
            "Alex",
            capabilities_for_request(true, true),
            ApprovalScope::ThreeHours,
            T0,
        )
        .unwrap();
        assert!(s.is_allowed(TECH, Capability::Elevated, T0 + THREE_HOURS_SECS - 1));
        assert!(!s.is_allowed(TECH, Capability::Elevated, T0 + THREE_HOURS_SECS));
    }

    #[test]
    fn a_legacy_grant_is_never_read_as_elevated() {
        // A grant persisted before this rung existed has no `capabilities`
        // field at all. It must load as view-only — an absent field widening
        // all the way to administrator would be the worst possible default.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("consent.json");
        let legacy = format!(
            r#"{{ "version": 1, "grants": [
                {{ "technician": "{TECH}", "agent_name": "Alex",
                   "scope": "forever", "granted_at": {T0} }}
            ] }}"#
        );
        std::fs::write(&path, legacy).unwrap();
        let s = ConsentStore::load(&path);
        assert!(s.is_allowed(TECH, Capability::ScreenView, T0));
        assert!(!s.is_allowed(TECH, Capability::Control, T0));
        assert!(!s.is_allowed(TECH, Capability::Elevated, T0));
    }

    #[test]
    fn elevated_grants_round_trip_through_disk() {
        // Both halves must survive a restart together: the machine's install-time
        // decision AND the technician's grant. If either failed to reload the
        // customer would be re-prompted (or silently downgraded) after a reboot
        // mid-repair, which is exactly what the install-once model exists to avoid.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("consent.json");
        {
            let mut s = ConsentStore::load(&path);
            s.set_elevation_policy(true, T0).unwrap();
            s.approve(
                TECH,
                "Alex",
                capabilities_for_request(true, true),
                ApprovalScope::Forever,
                T0,
            )
            .unwrap();
        }
        let reloaded = ConsentStore::load(&path);
        assert!(reloaded.elevation_allowed());
        assert!(reloaded.is_allowed(TECH, Capability::Elevated, T0));
        assert!(reloaded.active_grants(T0)[0].allows(Capability::Elevated));
    }
}
