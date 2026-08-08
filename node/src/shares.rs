//! Shares — the persisted answer to "who am I sharing with, and for what?"
//!
//! Where [`crate::ownership::Ownership`] remembers *my own* fleet, this
//! remembers my **share relationships with other people** — durable across
//! restarts, exactly like ownership, and stored right beside it under
//! `~/.myownmesh` (`MYOWNMESH_HOME`-overridable). It is the node-side source
//! of truth: enforcement lives in the node, so the durable record of what's
//! allowed must live here too (the GUI projects it and drives it, the way it
//! does for ownership). A fresh start therefore reclassifies a peer as
//! *shared* with its grants instead of forgetting them — the durability gap
//! that made sharing no better than a room.
//!
//! A share is with a **person**, not one machine (people bring fleets), and
//! its grants split by who authored them:
//!
//!  * **outbound** (`out_grants`) — grants *I* extended: what this person may
//!    do with *my* stuff ("Alex may receive my screen"). Minted only by my own
//!    explicit action.
//!  * **inbound** (`in_grants`) — grants *they* extended to me: what I may do
//!    with *their* stuff ("I may receive Alex's camera"). Safe to record from
//!    an authenticated peer, because they only ever widen what I may pull from
//!    them, never what they may do to me.
//!
//! Authorization unions the two — the grant's `role` (`Provide`/`Consume`)
//! already encodes direction — so the projected [`Share`] feeds the existing
//! `Catalog` gate unchanged. The split exists for the negotiation trust rule
//! (an inbound offer must never mint an outbound grant) and the UI's two
//! sides; it is invisible to the route check.

use std::path::PathBuf;

use allmystuff_graph::{Grant, NodeId, Person, PersonId, Share};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// One durable share relationship with a person. Additive + `#[serde(default)]`
/// so an older file (or none) still loads.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedShare {
    person: Person,
    /// The peer nodes this person brings, in canonical-pubkey form, so a
    /// device of theirs that reconnects (or appears in display form) re-binds
    /// to the same share.
    #[serde(default)]
    nodes: Vec<NodeId>,
    /// Grants *I* extended — what they may do with my stuff.
    #[serde(default)]
    out_grants: Vec<Grant>,
    /// Grants *they* extended to me — what I may do with their stuff.
    #[serde(default)]
    in_grants: Vec<Grant>,
}

/// The durable part of the record.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Persisted {
    /// Bumped on every local edit — the seed for revoke/convergence ordering
    /// once shares gossip (mirrors the fleet roster's version).
    #[serde(default)]
    version: u64,
    #[serde(default)]
    shares: Vec<PersistedShare>,
}

/// Live state behind one lock, so an edit's read-modify-write is atomic.
#[derive(Debug, Default)]
struct Inner {
    version: u64,
    shares: Vec<PersistedShare>,
}

/// The live shares store. Cheap to share behind an `Arc`.
pub struct Shares {
    path: Option<PathBuf>,
    inner: Mutex<Inner>,
}

impl Shares {
    /// Load the record from disk (or start blank).
    pub fn load() -> Self {
        Self::load_at(store_path())
    }

    /// Load from an explicit path (`None` = in-memory, every `persist` a
    /// no-op "ok"). The seam the tests use, and what [`Shares::load`] funnels
    /// through.
    fn load_at(path: Option<PathBuf>) -> Self {
        // Missing → blank; corrupt → quarantined + blank, loudly (silently
        // blank would drop every standing share without a trace).
        let persisted: Persisted = path
            .as_ref()
            .map(|p| crate::persist::load_json(p))
            .unwrap_or_default();
        Shares {
            path,
            inner: Mutex::new(Inner {
                version: persisted.version,
                shares: persisted.shares,
            }),
        }
    }

    /// Every share, projected to the graph's [`Share`] shape (grants unioned
    /// across inbound + outbound) — what the snapshot hands the GUI and what
    /// stamps a peer's [`allmystuff_graph::Relationship::Shared`].
    pub fn shares(&self) -> Vec<Share> {
        let i = self.inner.lock();
        i.shares.iter().map(project).collect()
    }

    /// The person a peer node belongs to, if it's part of a share — the hook
    /// the enforcement gates use to ask "does a grant cover this?".
    pub fn person_for_node(&self, node: &str) -> Option<Person> {
        let canon = pubkey_part(node);
        let i = self.inner.lock();
        i.shares
            .iter()
            .find(|s| s.nodes.iter().any(|n| pubkey_part(n.as_str()) == canon))
            .map(|s| s.person.clone())
    }

    /// The union of grants (inbound + outbound) in this person's share —
    /// every grant that could authorize a route involving their devices.
    pub fn grants_for(&self, person: &PersonId) -> Vec<Grant> {
        let i = self.inner.lock();
        i.shares
            .iter()
            .find(|s| &s.person.id == person)
            .map(union_grants)
            .unwrap_or_default()
    }

    /// The **outbound** grants this person holds from me — what they may do
    /// with my stuff. This is the authoritative set an [`ShareControl::Invite`]
    /// carries to the peer (sent whole, since the peer records inbound by
    /// replacement), and the set a full "stop sharing" must revoke on the wire.
    pub fn out_grants_for(&self, person: &PersonId) -> Vec<Grant> {
        let i = self.inner.lock();
        i.shares
            .iter()
            .find(|s| &s.person.id == person)
            .map(|s| s.out_grants.clone())
            .unwrap_or_default()
    }

    /// The peer nodes this person brings (canonical pubkey form) — who to tell
    /// when a grant changes, so every device of theirs converges.
    pub fn nodes_for(&self, person: &PersonId) -> Vec<NodeId> {
        let i = self.inner.lock();
        i.shares
            .iter()
            .find(|s| &s.person.id == person)
            .map(|s| s.nodes.clone())
            .unwrap_or_default()
    }

    /// Record an **outbound** grant — what this person may do with my stuff.
    /// Authored by me; de-duped by the grant's (content-derived) id so two
    /// structurally identical grants collapse to one. Returns whether the
    /// stored set changed.
    pub fn grant(&self, person: &Person, node: &NodeId, grant: Grant) -> bool {
        let mut i = self.inner.lock();
        let s = ensure_share(&mut i.shares, person);
        add_node(s, node);
        let changed = upsert_grant(&mut s.out_grants, grant);
        if changed {
            i.version += 1;
            persist(&self.path, &i);
        }
        changed
    }

    /// Replace this person's **inbound** grant set with what a peer extended
    /// to me — what I may do with their stuff. Safe from an authenticated
    /// peer (it only widens what I may pull from them). Returns whether the
    /// stored set changed.
    pub fn record_inbound(&self, person: &Person, node: &NodeId, grants: Vec<Grant>) -> bool {
        let mut i = self.inner.lock();
        let s = ensure_share(&mut i.shares, person);
        add_node(s, node);
        if s.in_grants == grants {
            return false;
        }
        s.in_grants = grants;
        i.version += 1;
        persist(&self.path, &i);
        true
    }

    /// Revoke a grant by id from this person's share (either direction).
    /// The id is content-derived, so it names the same grant across a restart
    /// and on both peers. Returns whether anything was removed.
    pub fn revoke(&self, person: &PersonId, grant_id: &str) -> bool {
        let mut i = self.inner.lock();
        let Some(s) = i.shares.iter_mut().find(|s| &s.person.id == person) else {
            return false;
        };
        let before = s.out_grants.len() + s.in_grants.len();
        s.out_grants.retain(|g| g.id != grant_id);
        s.in_grants.retain(|g| g.id != grant_id);
        let removed = s.out_grants.len() + s.in_grants.len() != before;
        if removed {
            i.version += 1;
            persist(&self.path, &i);
        }
        removed
    }

    /// Drop a person's whole share — the durable "stop sharing with Alex".
    /// Returns whether a record was removed.
    pub fn stop_sharing(&self, person: &PersonId) -> bool {
        let mut i = self.inner.lock();
        let before = i.shares.len();
        i.shares.retain(|s| &s.person.id != person);
        let removed = i.shares.len() != before;
        if removed {
            i.version += 1;
            persist(&self.path, &i);
        }
        removed
    }
}

/// Project a stored share to the graph's [`Share`] (grants unioned).
fn project(s: &PersistedShare) -> Share {
    Share {
        person: s.person.clone(),
        grants: union_grants(s),
    }
}

/// Outbound + inbound grants as one set, de-duped by id (the union the
/// `Catalog` checks; role already encodes direction).
fn union_grants(s: &PersistedShare) -> Vec<Grant> {
    let mut out = s.out_grants.clone();
    for g in &s.in_grants {
        if !out.iter().any(|x| x.id == g.id) {
            out.push(g.clone());
        }
    }
    out
}

/// The mutable share for `person`, created if absent; refreshes a non-empty
/// display name. Uses an owned index so the lookup borrow is released before
/// the push.
fn ensure_share<'a>(
    shares: &'a mut Vec<PersistedShare>,
    person: &Person,
) -> &'a mut PersistedShare {
    match shares.iter().position(|s| s.person.id == person.id) {
        Some(pos) => {
            if !person.name.is_empty() {
                shares[pos].person.name = person.name.clone();
            }
            &mut shares[pos]
        }
        None => {
            shares.push(PersistedShare {
                person: person.clone(),
                nodes: Vec::new(),
                out_grants: Vec::new(),
                in_grants: Vec::new(),
            });
            shares.last_mut().expect("just pushed")
        }
    }
}

/// Add `node` to a share's device list, matching by canonical pubkey so a
/// machine in display form and bare form never doubles up.
fn add_node(s: &mut PersistedShare, node: &NodeId) {
    let canon = pubkey_part(node.as_str());
    if !s.nodes.iter().any(|n| pubkey_part(n.as_str()) == canon) {
        s.nodes.push(NodeId::from(canon));
    }
}

/// Insert or replace `grant` by its (content-derived) id. Returns whether the
/// list changed.
fn upsert_grant(grants: &mut Vec<Grant>, grant: Grant) -> bool {
    match grants.iter().position(|g| g.id == grant.id) {
        Some(pos) => {
            if grants[pos] != grant {
                grants[pos] = grant;
                true
            } else {
                false
            }
        }
        None => {
            grants.push(grant);
            true
        }
    }
}

/// The stable pubkey portion of a mesh device id — strip MyOwnMesh's trailing
/// 5-char display suffix (`-AB12C`). Mirrors `ownership::pubkey_part` /
/// `mesh::pubkey_part` so a device in display form and bare form collapse to
/// one share member.
fn pubkey_part(id: &str) -> &str {
    if let Some((body, suffix)) = id.rsplit_once('-') {
        if suffix.len() == 5 && suffix.chars().all(|c| c.is_ascii_alphanumeric()) {
            return body;
        }
    }
    id
}

/// Write the durable part. Returns whether it was saved (a missing home dir —
/// an ephemeral/test environment — counts as "nothing to persist, ok").
fn persist(path: &Option<PathBuf>, inner: &Inner) -> bool {
    let Some(path) = path else { return true };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let persisted = Persisted {
        version: inner.version,
        shares: inner.shares.clone(),
    };
    match serde_json::to_string_pretty(&persisted) {
        Ok(json) => crate::persist::write_atomic(path, json.as_bytes()).is_ok(),
        Err(_) => false,
    }
}

/// `~/.myownmesh/allmystuff-shares.json`, honouring `MYOWNMESH_HOME` — the
/// same home the control socket, identity, and ownership record use.
fn store_path() -> Option<PathBuf> {
    Some(allmystuff_protocol::myownmesh_state_dir()?.join("allmystuff-shares.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use allmystuff_graph::{GrantRole, MediaKind};

    /// A store with no backing file — every `persist` is a no-op "ok".
    fn memory() -> Shares {
        Shares::load_at(None)
    }

    fn alex() -> Person {
        Person {
            id: "person:alex".into(),
            name: "Alex".into(),
        }
    }

    fn screen_to_alex() -> Grant {
        // Outbound: Alex may receive my screen.
        Grant::scoped(
            &alex().id,
            MediaKind::Display,
            GrantRole::Consume,
            None,
            "Receive your screen",
        )
    }

    #[test]
    fn an_outbound_grant_is_recorded_and_dedups_by_id() {
        let sh = memory();
        let node = NodeId::from("alexkey-AAAAA");
        assert!(sh.grant(&alex(), &node, screen_to_alex()));
        // The exact same grant again is a no-op.
        assert!(!sh.grant(&alex(), &node, screen_to_alex()));
        // Same scope, new label → same id: refreshes in place (still one
        // grant, newest label wins) rather than adding a duplicate.
        let relabelled = Grant::scoped(
            &alex().id,
            MediaKind::Display,
            GrantRole::Consume,
            None,
            "Cast to them",
        );
        assert!(sh.grant(&alex(), &node, relabelled));
        let grants = sh.grants_for(&alex().id);
        assert_eq!(grants.len(), 1, "de-duped by content-derived id");
        assert_eq!(grants[0].media, MediaKind::Display);
        assert_eq!(grants[0].label, "Cast to them");
    }

    #[test]
    fn revoke_removes_by_id_and_stop_sharing_drops_the_person() {
        let sh = memory();
        let node = NodeId::from("alex");
        sh.grant(&alex(), &node, screen_to_alex());
        assert!(sh.revoke(&alex().id, &screen_to_alex().id));
        assert!(sh.grants_for(&alex().id).is_empty());
        // Re-add, then stop sharing with the person entirely.
        sh.grant(&alex(), &node, screen_to_alex());
        assert!(sh.stop_sharing(&alex().id));
        assert!(sh.shares().is_empty());
        // Idempotent: nothing left to stop or revoke.
        assert!(!sh.stop_sharing(&alex().id));
        assert!(!sh.revoke(&alex().id, &screen_to_alex().id));
    }

    #[test]
    fn person_for_node_collapses_display_and_bare_pubkey() {
        let sh = memory();
        sh.grant(&alex(), &NodeId::from("alexkey-AB12C"), screen_to_alex());
        // The same machine in either form resolves to the same person.
        assert_eq!(sh.person_for_node("alexkey").unwrap().id, alex().id);
        assert_eq!(sh.person_for_node("alexkey-AB12C").unwrap().id, alex().id);
        assert!(sh.person_for_node("stranger").is_none());
    }

    #[test]
    fn inbound_and_outbound_grants_union_in_the_projection() {
        let sh = memory();
        let node = NodeId::from("alex");
        sh.grant(&alex(), &node, screen_to_alex()); // outbound
        let cam = Grant::scoped(
            &alex().id,
            MediaKind::Video,
            GrantRole::Provide,
            None,
            "Send their camera",
        );
        assert!(sh.record_inbound(&alex(), &node, vec![cam]));
        // Same inbound set again → no change.
        let cam_again = Grant::scoped(
            &alex().id,
            MediaKind::Video,
            GrantRole::Provide,
            None,
            "Send their camera",
        );
        assert!(!sh.record_inbound(&alex(), &node, vec![cam_again]));

        let projected = sh.shares();
        assert_eq!(projected.len(), 1);
        // Both directions present in the one Share the catalog gate sees.
        assert_eq!(projected[0].grants.len(), 2);
        assert_eq!(projected[0].person.id, alex().id);
    }

    #[test]
    fn out_grants_and_nodes_back_the_wire_invite() {
        let sh = memory();
        let node = NodeId::from("alexkey-AB12C");
        sh.grant(&alex(), &node, screen_to_alex()); // outbound
        let cam = Grant::scoped(
            &alex().id,
            MediaKind::Video,
            GrantRole::Provide,
            None,
            "Send their camera",
        );
        sh.record_inbound(&alex(), &node, vec![cam]); // inbound, not mine to offer

        // An Invite carries only what *I* extend (outbound), never what they
        // extended to me — otherwise I'd "offer" their own grant back at them.
        let out = sh.out_grants_for(&alex().id);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].media, MediaKind::Display);

        // And the devices to send it to, in canonical-pubkey form.
        let nodes = sh.nodes_for(&alex().id);
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].as_str(), "alexkey");

        // Unknown person → empty, never a panic.
        assert!(sh.out_grants_for(&"person:nobody".into()).is_empty());
        assert!(sh.nodes_for(&"person:nobody".into()).is_empty());
    }

    #[test]
    fn grants_survive_a_reload_from_disk() {
        let dir = std::env::temp_dir().join(format!("ams-shares-test-{}", unique()));
        let path = Some(dir.join("allmystuff-shares.json"));
        {
            let sh = Shares::load_at(path.clone());
            assert!(sh.grant(&alex(), &NodeId::from("alexkey-AAAAA"), screen_to_alex()));
        }
        // A fresh load from the same path still sees the grant — the
        // durability the whole feature turns on.
        let reloaded = Shares::load_at(path);
        let grants = reloaded.grants_for(&alex().id);
        assert_eq!(grants.len(), 1);
        assert_eq!(reloaded.person_for_node("alexkey").unwrap().id, alex().id);
        let _ = std::fs::remove_dir_all(dir);
    }

    fn unique() -> u128 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }
}
