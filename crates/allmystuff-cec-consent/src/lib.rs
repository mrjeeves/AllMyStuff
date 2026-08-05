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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// See the customer's screen.
    ScreenView,
    /// Drive the customer's keyboard and mouse (implies [`ScreenView`]).
    ///
    /// [`ScreenView`]: Capability::ScreenView
    Control,
}

impl Capability {
    /// Whether holding `self` satisfies a request for `wanted`. `Control`
    /// implies `ScreenView`; `ScreenView` does not imply `Control`.
    fn covers(self, wanted: Capability) -> bool {
        self == wanted || (self == Capability::Control && wanted == Capability::ScreenView)
    }
}

/// The safe minimum a grant carries when its `capabilities` field is absent
/// (an older persisted grant): view-only, never control. A missing field must
/// never widen access.
fn default_capabilities() -> Vec<Capability> {
    vec![Capability::ScreenView]
}

/// Map the wire `want_control` flag to the capability set a grant should carry.
pub fn capabilities_for(want_control: bool) -> Vec<Capability> {
    if want_control {
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

/// On-disk shape. Only persistent grants (`ThreeHours`, `Forever`) are written;
/// `Once` grants live in [`ConsentStore::ephemeral`] and never touch disk.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Persisted {
    #[serde(default)]
    version: u32,
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
    pub fn is_allowed(&self, technician: &str, cap: Capability, now: u64) -> bool {
        let key = pubkey_part(technician);
        self.persistent
            .iter()
            .chain(self.ephemeral.iter())
            .any(|g| g.technician == key && g.is_live(now) && g.covers(cap))
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
}
