//! Device ownership — the persisted answer to "whose machine is this?"
//!
//! AllMyStuff's rule (the one the user asked for): you can't *flat-out take
//! ownership* of a device on the mesh. A claim only takes if the device is
//! in **claim mode** — started with `ALLMYSTUFF_CLAIMABLE=1`, or with its
//! owner having toggled "allow adoption" in its own UI. Otherwise the device
//! defines its own owner (whatever it has recorded, or nobody), and a peer
//! can look but not grab.
//!
//! Two pieces of state, treated differently on purpose:
//!
//!  * **owner** — *persisted* next to the mesh identity under `~/.myownmesh`
//!    (`MYOWNMESH_HOME`-overridable), so a device remembers who owns it
//!    across restarts exactly like the mesh remembers its keys.
//!  * **claim mode** — *not* persisted. It's a transient "I'm offering this
//!    device right now" state, re-asserted each start by the
//!    `ALLMYSTUFF_CLAIMABLE` flag, so a box never sits silently adoptable
//!    across reboots after you toggled it on once.

use std::path::PathBuf;

use allmystuff_graph::NodeId;
use allmystuff_protocol::{OwnedMember, OwnedRoster};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// The durable part of the record. The owner *and* the owned-fleet roster
/// survive a restart (only claim mode is transient). Additive +
/// `#[serde(default)]` so an older file (or none) still loads.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Persisted {
    #[serde(default)]
    owner: Option<String>,
    /// The shared key of the fleet this device belongs to, if any. Minted the
    /// first time this device claims another, or handed down by its owner when
    /// this device is itself adopted.
    #[serde(default)]
    fleet_key: Option<String>,
    /// The fleet's display name ("Casey") — cosmetic, gossiped with the
    /// roster, empty when unnamed.
    #[serde(default)]
    fleet_name: String,
    /// Last version of the owned roster we hold (last-writer-wins on gossip).
    #[serde(default)]
    fleet_version: u64,
    /// The fleet's members — every co-owned device, in canonical-pubkey form.
    #[serde(default)]
    fleet_members: Vec<OwnedMember>,
}

/// Live state behind one lock, so a claim's check-and-set is atomic.
#[derive(Debug, Default)]
struct Inner {
    owner: Option<String>,
    claim_mode: bool,
    fleet_key: Option<String>,
    fleet_name: String,
    fleet_version: u64,
    fleet_members: Vec<OwnedMember>,
}

/// The live ownership store. Cheap to share behind an `Arc`.
pub struct Ownership {
    path: Option<PathBuf>,
    inner: Mutex<Inner>,
}

impl Ownership {
    /// Load the record from disk (or start blank). The start-time
    /// `ALLMYSTUFF_CLAIMABLE` flag seeds claim mode for this run, but only
    /// while the device is still unowned.
    pub fn load() -> Self {
        let path = store_path();
        let persisted = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str::<Persisted>(&s).ok())
            .unwrap_or_default();
        let inner = Inner {
            claim_mode: persisted.owner.is_none() && env_claim_flag(),
            owner: persisted.owner,
            fleet_key: persisted.fleet_key,
            fleet_name: persisted.fleet_name,
            fleet_version: persisted.fleet_version,
            fleet_members: persisted.fleet_members,
        };
        Ownership {
            path,
            inner: Mutex::new(inner),
        }
    }

    /// The recorded owner's node id, if any.
    pub fn owner(&self) -> Option<String> {
        self.inner.lock().owner.clone()
    }

    /// Whether this device is currently offering itself for adoption: no
    /// owner yet **and** claim mode is on. This is the *only* condition under
    /// which a claim is accepted.
    pub fn claimable(&self) -> bool {
        let i = self.inner.lock();
        i.owner.is_none() && i.claim_mode
    }

    /// Record (or clear) the owner. Recording one ends claim mode — an owned
    /// device is never claimable until its owner releases it. Clearing the
    /// owner (a release) also leaves the fleet: membership follows
    /// ownership, and a kept key would leave this device deaf to its *next*
    /// owner's roster gossip. Returns whether the durable write succeeded.
    pub fn set_owner(&self, owner: Option<String>) -> bool {
        let mut i = self.inner.lock();
        i.owner = owner;
        if i.owner.is_some() {
            i.claim_mode = false;
        } else {
            i.fleet_key = None;
            i.fleet_version = 0;
            i.fleet_members.clear();
        }
        persist(&self.path, &i)
    }

    /// Turn claim mode on or off at runtime (only meaningful while unowned).
    pub fn set_claim_mode(&self, on: bool) {
        let mut i = self.inner.lock();
        i.claim_mode = on && i.owner.is_none();
    }

    /// Accept a claim from `claimer` — but only if the device is currently
    /// claimable **and** the new owner can be durably recorded. Both the
    /// check and the set happen under one lock so a claim can't race another
    /// or be acknowledged without being persisted. Returns whether it took.
    ///
    /// Accepting also **resets any fleet state**: this device is joining its
    /// new owner's fleet from scratch (the owner hands the roster down right
    /// after the claim). A key left over from an earlier ownership — or an
    /// owner who has since re-minted identity — has a different fleet key,
    /// and [`Ownership::merge_fleet`] would ignore the new owner's gossip
    /// forever, which is exactly the "claimed, but the fleet never shows up
    /// on the device" failure.
    pub fn try_accept_claim(&self, claimer: &str) -> bool {
        let mut i = self.inner.lock();
        if i.owner.is_some() || !i.claim_mode {
            return false;
        }
        i.owner = Some(claimer.to_string());
        i.claim_mode = false;
        let prev_key = i.fleet_key.take();
        let prev_name = std::mem::take(&mut i.fleet_name);
        let prev_version = std::mem::take(&mut i.fleet_version);
        let prev_members = std::mem::take(&mut i.fleet_members);
        if persist(&self.path, &i) {
            true
        } else {
            // Couldn't record it durably — roll back rather than pretend the
            // claim took (the peer would be told Claimed, then we'd come back
            // unowned after a restart).
            i.owner = None;
            i.claim_mode = true;
            i.fleet_key = prev_key;
            i.fleet_name = prev_name;
            i.fleet_version = prev_version;
            i.fleet_members = prev_members;
            false
        }
    }

    // ---- owned fleet (the gossiped "Owned" roster) ----------------------
    //
    // Claiming a device links the two machines under a shared **fleet key**.
    // The owner mints the key on its first claim; every adoption adds the new
    // device to the roster and hands the key down. All co-owned devices gossip
    // the [`OwnedRoster`] on `CHANNEL_OWNED` and converge by version, so the
    // fleet is the same set everywhere. For now this only groups devices
    // internally — a later edition links the key to other things.

    /// Drop the held fleet when it's incoherent with who we are now: a
    /// roster this device isn't even a member of, or (when owned) one that
    /// doesn't include our owner. Either is residue from an earlier life —
    /// an old ownership, a re-minted identity, a pre-fix bystander adoption
    /// — and holding its key would leave this device deaf to the real
    /// fleet's gossip. Run once at session start, when `me` is known.
    /// Returns whether anything was dropped.
    pub fn sanitize_fleet(&self, me: &str) -> bool {
        let mut i = self.inner.lock();
        if i.fleet_key.is_none() {
            return false;
        }
        let listed = |id: &str| {
            let canon = pubkey_part(id);
            i.fleet_members
                .iter()
                .any(|m| pubkey_part(m.device.as_str()) == canon)
        };
        let coherent = listed(me)
            && match &i.owner {
                Some(o) => listed(o),
                None => true,
            };
        if coherent {
            return false;
        }
        i.fleet_key = None;
        i.fleet_name.clear();
        i.fleet_version = 0;
        i.fleet_members.clear();
        persist(&self.path, &i);
        true
    }

    /// The fleet roster this device currently holds, if it belongs to a fleet.
    pub fn fleet(&self) -> Option<OwnedRoster> {
        let i = self.inner.lock();
        let key = i.fleet_key.clone()?;
        Some(OwnedRoster {
            key,
            name: i.fleet_name.clone(),
            version: i.fleet_version,
            members: i.fleet_members.clone(),
        })
    }

    /// Make sure this device has a fleet key, minting a fresh one the first
    /// time (e.g. when it claims its first device). Returns the key.
    pub fn ensure_fleet_key(&self) -> String {
        let mut i = self.inner.lock();
        if i.fleet_key.is_none() {
            i.fleet_key = Some(new_fleet_key());
            i.fleet_version = i.fleet_version.max(1);
            persist(&self.path, &i);
        }
        i.fleet_key.clone().unwrap_or_default()
    }

    /// Add or refresh a member of this device's fleet, bumping the version.
    /// Members are keyed by canonical pubkey so one machine never doubles up.
    /// Returns whether the roster actually changed.
    pub fn upsert_member(&self, device: &str, label: &str) -> bool {
        let mut i = self.inner.lock();
        let changed = upsert_member_into(&mut i.fleet_members, device, label);
        if changed {
            i.fleet_version += 1;
            persist(&self.path, &i);
        }
        changed
    }

    /// Merge an inbound fleet roster a peer gossiped — but only from a sender
    /// we already trust. `from` is the daemon-attested peer id of the sender.
    /// Membership grants owner/fleet-only control on the mesh (terminal, files,
    /// input), so a roster is honoured **only** when `from` is our recorded
    /// owner or an already-known fleet member. A keyless device adopting its
    /// first fleet accepts it solely from its **owner** (the peer it just
    /// completed the authenticated claim with) and only when the roster lists
    /// us; a device that already holds a key still requires the key to match
    /// *and* the sender to be owner/member — knowing the (broadcast-observable)
    /// key is not enough. This is the gate that stops a stranger writing
    /// themselves into the fleet to earn a remote shell. Once we have a key, a
    /// foreign fleet's gossip (different key) is ignored.
    ///
    /// Convergence is by version, with *replacement* semantics on a newer
    /// roster: a strictly newer copy replaces our member set wholesale —
    /// that's how a **leave or kick propagates** (a union can only ever
    /// add). If the newer set no longer lists us, we've been kicked and
    /// drop the fleet entirely. Equal versions union (two members adding
    /// concurrently heal into one set, label refreshes ride along); older
    /// gossip is ignored — our next broadcast corrects the sender.
    ///
    /// `me` is this device's own id (any suffix form).
    ///
    /// Returns whether the merge was **structural** — key adopted, member
    /// set changed, or we were kicked — the signal to re-broadcast (when a
    /// fleet remains) and refresh the UI. A label-only refresh is saved but
    /// does *not* re-broadcast, so two peers that disagree on a label can't
    /// ping-pong gossip forever.
    pub fn merge_fleet(&self, me: &str, from: &str, incoming: &OwnedRoster) -> bool {
        if incoming.key.is_empty() {
            return false;
        }
        let me_canon = pubkey_part(me).to_string();
        let from_canon = pubkey_part(from);
        if from_canon.is_empty() {
            return false;
        }
        let mut i = self.inner.lock();
        // Authorization gate — the fix for unauthenticated fleet-roster
        // injection. A roster is only honoured from a sender we already trust,
        // never from an arbitrary peer that merely lists us or merely knows the
        // (broadcast-observable) key. Computed before any mutation so the
        // immutable borrow doesn't conflict.
        let from_is_owner = i.owner.as_deref().map(pubkey_part) == Some(from_canon);
        let from_is_member = i
            .fleet_members
            .iter()
            .any(|m| pubkey_part(m.device.as_str()) == from_canon);
        match &i.fleet_key {
            // Already in a fleet: only our owner or an existing member may move
            // membership, and only within our own fleet (matching key).
            Some(k) => {
                if k != &incoming.key || !(from_is_owner || from_is_member) {
                    return false;
                }
            }
            // Keyless — a freshly-claimed device adopting its first fleet:
            // accept only from our recorded owner, and only when the roster
            // actually lists us. A device with no owner never adopts.
            None => {
                if !from_is_owner {
                    return false;
                }
                let listed = incoming
                    .members
                    .iter()
                    .any(|m| pubkey_part(m.device.as_str()) == me_canon);
                if !listed {
                    return false;
                }
            }
        }
        let adopting = i.fleet_key.is_none();

        if adopting || incoming.version > i.fleet_version {
            // Newer truth. Not listed any more → we've been kicked (or our
            // leave echoed back): drop the fleet outright.
            let listed = incoming
                .members
                .iter()
                .any(|m| pubkey_part(m.device.as_str()) == me_canon);
            if !listed {
                i.fleet_key = None;
                i.fleet_name.clear();
                i.fleet_version = 0;
                i.fleet_members.clear();
                persist(&self.path, &i);
                return true;
            }
            let new_members: Vec<OwnedMember> = incoming
                .members
                .iter()
                .map(|m| OwnedMember {
                    device: NodeId::from(pubkey_part(m.device.as_str())),
                    label: m.label.clone(),
                })
                .collect();
            let same_set = i.fleet_members.len() == new_members.len()
                && i.fleet_members.iter().all(|x| {
                    new_members
                        .iter()
                        .any(|n| n.device.as_str() == x.device.as_str())
                });
            // The fleet's name rides replacement like membership — a rename
            // alone is a structural change (re-broadcast + UI refresh).
            let renamed = i.fleet_name != incoming.name;
            i.fleet_key = Some(incoming.key.clone());
            i.fleet_name = incoming.name.clone();
            i.fleet_members = new_members;
            i.fleet_version = incoming.version;
            persist(&self.path, &i);
            return adopting || !same_set || renamed;
        }

        if incoming.version < i.fleet_version {
            // Stale gossip; our next broadcast brings the sender forward.
            return false;
        }

        // Equal versions: union members (concurrent adds heal), refresh
        // labels. A gained member makes ours strictly newer so our next
        // gossip out-ranks the copy we just merged.
        let mut structural = false;
        let mut dirty = false;
        if i.fleet_name.is_empty() && !incoming.name.is_empty() {
            // Name refresh, label-style: adopt when we have none (a
            // conflicting non-empty name is left to the next versioned
            // rename, never ping-ponged at equal versions).
            i.fleet_name = incoming.name.clone();
            dirty = true;
        }
        for m in &incoming.members {
            let canon = pubkey_part(m.device.as_str());
            match i
                .fleet_members
                .iter()
                .position(|x| pubkey_part(x.device.as_str()) == canon)
            {
                Some(pos) => {
                    if !m.label.is_empty() && i.fleet_members[pos].label != m.label {
                        i.fleet_members[pos].label = m.label.clone();
                        dirty = true; // label refresh — saved, but not re-broadcast
                    }
                }
                None => {
                    i.fleet_members.push(OwnedMember {
                        device: NodeId::from(canon),
                        label: m.label.clone(),
                    });
                    structural = true;
                    dirty = true;
                }
            }
        }
        if structural {
            i.fleet_version += 1;
        }
        if dirty {
            persist(&self.path, &i);
        }
        structural
    }

    /// Leave the fleet: returns the bumped roster *without us* — broadcast
    /// it so the remaining members converge on our absence — and clears our
    /// own fleet state. `None` when we weren't in a fleet to begin with.
    pub fn leave_fleet(&self, me: &str) -> Option<OwnedRoster> {
        let me_canon = pubkey_part(me).to_string();
        let mut i = self.inner.lock();
        let key = i.fleet_key.clone()?;
        if !i
            .fleet_members
            .iter()
            .any(|m| pubkey_part(m.device.as_str()) == me_canon)
        {
            return None;
        }
        let members: Vec<OwnedMember> = i
            .fleet_members
            .iter()
            .filter(|m| pubkey_part(m.device.as_str()) != me_canon)
            .cloned()
            .collect();
        let roster = OwnedRoster {
            key,
            name: i.fleet_name.clone(),
            version: i.fleet_version + 1,
            members,
        };
        i.fleet_key = None;
        i.fleet_name.clear();
        i.fleet_version = 0;
        i.fleet_members.clear();
        persist(&self.path, &i);
        Some(roster)
    }

    /// Remove `device` from the fleet. Only a member may kick — you can't
    /// kick others from a fleet you aren't in — and removing *yourself* is
    /// [`Ownership::leave_fleet`]. Returns the bumped roster to broadcast.
    pub fn kick_member(&self, me: &str, device: &str) -> Result<OwnedRoster, String> {
        let mut i = self.inner.lock();
        let Some(key) = i.fleet_key.clone() else {
            return Err("this device isn't in a fleet".into());
        };
        let listed = |members: &[OwnedMember], id: &str| {
            let canon = pubkey_part(id);
            members
                .iter()
                .any(|m| pubkey_part(m.device.as_str()) == canon)
        };
        if !listed(&i.fleet_members, me) {
            return Err("you can't kick devices from a fleet you aren't in".into());
        }
        if pubkey_part(me) == pubkey_part(device) {
            return Err("use Leave to remove this device".into());
        }
        if !listed(&i.fleet_members, device) {
            return Err("that device isn't in the fleet".into());
        }
        let canon = pubkey_part(device).to_string();
        i.fleet_members
            .retain(|m| pubkey_part(m.device.as_str()) != canon);
        i.fleet_version += 1;
        persist(&self.path, &i);
        Ok(OwnedRoster {
            key,
            name: i.fleet_name.clone(),
            version: i.fleet_version,
            members: i.fleet_members.clone(),
        })
    }

    /// Name (or rename) the fleet. Membership is the permission, the same
    /// rule as kicking: you can't name a fleet you aren't in. Bumps the
    /// version so the rename replaces everywhere the roster gossips, and
    /// returns the roster to broadcast.
    pub fn set_fleet_name(&self, me: &str, name: &str) -> Result<OwnedRoster, String> {
        let name = name.trim();
        let mut i = self.inner.lock();
        let Some(key) = i.fleet_key.clone() else {
            return Err("this device isn't in a fleet".into());
        };
        let me_canon = pubkey_part(me);
        if !i
            .fleet_members
            .iter()
            .any(|m| pubkey_part(m.device.as_str()) == me_canon)
        {
            return Err("you can't name a fleet you aren't in".into());
        }
        if i.fleet_name != name {
            i.fleet_name = name.to_string();
            i.fleet_version += 1;
            persist(&self.path, &i);
        }
        Ok(OwnedRoster {
            key,
            name: i.fleet_name.clone(),
            version: i.fleet_version,
            members: i.fleet_members.clone(),
        })
    }
}

/// Add or refresh `device` in `members`, matching by canonical pubkey. The
/// newest non-empty label wins. Returns whether the list changed. Uses an
/// owned index (`position`) so the lookup borrow is released before the push.
fn upsert_member_into(members: &mut Vec<OwnedMember>, device: &str, label: &str) -> bool {
    let canon = pubkey_part(device);
    match members
        .iter()
        .position(|m| pubkey_part(m.device.as_str()) == canon)
    {
        Some(pos) => {
            if !label.is_empty() && members[pos].label != label {
                members[pos].label = label.to_string();
                true
            } else {
                false
            }
        }
        None => {
            members.push(OwnedMember {
                device: NodeId::from(canon),
                label: label.to_string(),
            });
            true
        }
    }
}

/// Mint a fresh fleet key: 32 bytes of system randomness, hex-encoded. This
/// is an opaque grouping secret today; it carries no structure other apps
/// rely on.
fn new_fleet_key() -> String {
    let mut bytes = [0u8; 32];
    // The system RNG not being available is catastrophic and vanishingly
    // unlikely; failing loudly beats minting a predictable fleet key.
    getrandom::getrandom(&mut bytes).expect("system RNG unavailable for fleet key");
    let mut s = String::with_capacity(64);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// The stable pubkey portion of a mesh device id — strip MyOwnMesh's trailing
/// 5-char display suffix (`-AB12C`). Mirrors `mesh::pubkey_part` so a device
/// in display form and bare form collapse to one fleet member.
fn pubkey_part(id: &str) -> &str {
    if let Some((body, suffix)) = id.rsplit_once('-') {
        if suffix.len() == 5 && suffix.chars().all(|c| c.is_ascii_alphanumeric()) {
            return body;
        }
    }
    id
}

/// Write the durable part. Returns whether it was saved (a missing home dir —
/// e.g. an ephemeral/test environment — counts as "nothing to persist, ok").
fn persist(path: &Option<PathBuf>, inner: &Inner) -> bool {
    let Some(path) = path else { return true };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let persisted = Persisted {
        owner: inner.owner.clone(),
        fleet_key: inner.fleet_key.clone(),
        fleet_name: inner.fleet_name.clone(),
        fleet_version: inner.fleet_version,
        fleet_members: inner.fleet_members.clone(),
    };
    match serde_json::to_string_pretty(&persisted) {
        Ok(json) => std::fs::write(path, json).is_ok(),
        Err(_) => false,
    }
}

/// `~/.myownmesh/allmystuff-ownership.json`, honouring `MYOWNMESH_HOME` —
/// the same home the control socket and identity use.
fn store_path() -> Option<PathBuf> {
    let home = std::env::var_os("MYOWNMESH_HOME")
        .map(PathBuf::from)
        .or_else(dirs::home_dir)?;
    Some(home.join(".myownmesh").join("allmystuff-ownership.json"))
}

/// The start-time claim flag: `ALLMYSTUFF_CLAIMABLE` set to a truthy value.
fn env_claim_flag() -> bool {
    std::env::var("ALLMYSTUFF_CLAIMABLE")
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store with no backing file — every `persist` is a no-op "ok", so the
    /// fleet logic can be exercised without a home dir.
    fn memory() -> Ownership {
        Ownership {
            path: None,
            inner: Mutex::new(Inner::default()),
        }
    }

    #[test]
    fn pubkey_part_strips_the_display_suffix() {
        assert_eq!(pubkey_part("abcdef-AB12C"), "abcdef");
        // Not a 5-char alphanumeric suffix → left alone.
        assert_eq!(pubkey_part("abcdef-toolong"), "abcdef-toolong");
        assert_eq!(pubkey_part("bare"), "bare");
    }

    #[test]
    fn fleet_key_is_64_hex_chars_and_fresh_each_time() {
        let a = new_fleet_key();
        let b = new_fleet_key();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "two mints must not collide");
    }

    #[test]
    fn upsert_dedups_by_canonical_pubkey_and_refreshes_labels() {
        let mut members = Vec::new();
        assert!(upsert_member_into(&mut members, "k1-AB12C", "Laptop"));
        // Same machine via its bare pubkey: no new member, just maybe a label.
        assert!(!upsert_member_into(&mut members, "k1", "Laptop"));
        assert!(upsert_member_into(&mut members, "k1", "My laptop")); // label change
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].label, "My laptop");
    }

    #[test]
    fn claiming_builds_a_fleet_then_merges_converge() {
        // Owner mints a key and rosters itself + a claimed device.
        let owner = memory();
        let key = owner.ensure_fleet_key();
        assert!(!key.is_empty());
        assert!(owner.upsert_member("owner-AAAAA", "Owner"));
        assert!(owner.upsert_member("nuc-BBBBB", "Spare NUC"));
        let roster = owner.fleet().expect("owner has a fleet");
        assert_eq!(roster.members.len(), 2);

        // The claimed device records its owner (the claim handshake set it),
        // then merges the owner's gossip: it adopts the key and the membership
        // (it is listed).
        let target = memory();
        target.inner.lock().owner = Some("owner-AAAAA".into());
        assert!(target.merge_fleet("nuc-BBBBB", "owner-AAAAA", &roster));
        let t = target.fleet().unwrap();
        assert_eq!(t.key, key);
        assert_eq!(t.members.len(), 2);

        // A foreign fleet's gossip (different key) is ignored once we hold one.
        let foreign = OwnedRoster {
            key: "ffff".into(),
            name: String::new(),
            version: 99,
            members: vec![OwnedMember {
                device: "intruder".into(),
                label: "Intruder".into(),
            }],
        };
        assert!(!target.merge_fleet("nuc-BBBBB", "intruder", &foreign));
        assert_eq!(target.fleet().unwrap().members.len(), 2);
    }

    #[test]
    fn a_bystander_never_adopts_a_fleet_that_doesnt_list_it() {
        // Fleet gossip is broadcast — a keyless device on the same network
        // that is *not* in the roster must not join itself to the fleet.
        let roster = OwnedRoster {
            key: "k1".into(),
            name: String::new(),
            version: 3,
            members: vec![OwnedMember {
                device: "owner".into(),
                label: "Owner".into(),
            }],
        };
        let bystander = memory();
        bystander.inner.lock().owner = Some("owner".into());
        // Even from our own owner, a roster that doesn't list us isn't adopted.
        assert!(!bystander.merge_fleet("someone-else", "owner", &roster));
        assert!(bystander.fleet().is_none());
    }

    #[test]
    fn accepting_a_claim_resets_stale_fleet_state() {
        // The device holds a fleet key from an earlier life (previous owner,
        // re-minted identity, old test run). Accepting a new claim must drop
        // it — otherwise the *new* owner's roster gossip (a different key)
        // would be ignored forever and the fleet never shows up here.
        let dev = memory();
        dev.inner.lock().claim_mode = true;
        dev.inner.lock().fleet_key = Some("stale-key".into());
        dev.inner.lock().fleet_version = 9;
        dev.inner.lock().fleet_members.push(OwnedMember {
            device: "old-owner".into(),
            label: "Old".into(),
        });

        assert!(dev.try_accept_claim("new-owner"));
        assert!(dev.fleet().is_none(), "stale fleet must be gone");

        // The new owner's roster (fresh key, lists us) now adopts cleanly.
        let roster = OwnedRoster {
            key: "fresh-key".into(),
            name: String::new(),
            version: 3,
            members: vec![
                OwnedMember {
                    device: "new-owner".into(),
                    label: "Owner".into(),
                },
                OwnedMember {
                    device: "this-dev".into(),
                    label: "Me".into(),
                },
            ],
        };
        assert!(dev.merge_fleet("this-dev", "new-owner", &roster));
        assert_eq!(dev.fleet().unwrap().key, "fresh-key");
    }

    #[test]
    fn fleet_name_renames_version_and_gossips_with_replacement() {
        // The owner names the fleet: version bumps, the roster carries it.
        let owner = memory();
        owner.ensure_fleet_key();
        assert!(owner.upsert_member("owner-AAAAA", "Owner"));
        assert!(owner.upsert_member("nuc-BBBBB", "Spare NUC"));
        let before = owner.fleet().unwrap().version;
        let named = owner.set_fleet_name("owner-AAAAA", "  Casey  ").unwrap();
        assert_eq!(named.name, "Casey", "name is trimmed");
        assert_eq!(named.version, before + 1);

        // A member merging the newer roster adopts the name — and the
        // rename alone is structural (it re-broadcasts and refreshes UI).
        let member = memory();
        member.inner.lock().owner = Some("owner-AAAAA".into());
        assert!(member.merge_fleet("nuc-BBBBB", "owner-AAAAA", &named));
        assert_eq!(member.fleet().unwrap().name, "Casey");
        let renamed = OwnedRoster {
            name: "Casey's house".into(),
            version: named.version + 1,
            ..named.clone()
        };
        assert!(member.merge_fleet("nuc-BBBBB", "owner-AAAAA", &renamed));
        assert_eq!(member.fleet().unwrap().name, "Casey's house");

        // Equal-version gossip fills an *empty* name (label-style refresh,
        // not structural) but never overwrites a non-empty one.
        let other = memory();
        other.inner.lock().owner = Some("owner-AAAAA".into());
        let unnamed = OwnedRoster {
            name: String::new(),
            ..renamed.clone()
        };
        assert!(other.merge_fleet("nuc-BBBBB", "owner-AAAAA", &unnamed));
        assert!(
            !other.merge_fleet("nuc-BBBBB", "owner-AAAAA", &renamed),
            "name fill isn't structural"
        );
        assert_eq!(other.fleet().unwrap().name, "Casey's house");
        let conflicting = OwnedRoster {
            name: "Impostor".into(),
            ..renamed.clone()
        };
        assert!(!other.merge_fleet("nuc-BBBBB", "owner-AAAAA", &conflicting));
        assert_eq!(
            other.fleet().unwrap().name,
            "Casey's house",
            "equal versions never rename"
        );

        // A non-member can't name the fleet; renaming to the same name is
        // a no-op that doesn't bump the version.
        assert!(owner.set_fleet_name("stranger-XXXXX", "Hax").is_err());
        let v = owner.fleet().unwrap().version;
        let same = owner.set_fleet_name("owner-AAAAA", "Casey").unwrap();
        assert_eq!(same.version, v);
    }

    fn roster(key: &str, version: u64, devices: &[&str]) -> OwnedRoster {
        OwnedRoster {
            key: key.into(),
            name: String::new(),
            version,
            members: devices
                .iter()
                .map(|d| OwnedMember {
                    device: (*d).into(),
                    label: (*d).into(),
                })
                .collect(),
        }
    }

    #[test]
    fn a_newer_roster_replaces_membership_so_removals_propagate() {
        // A member holds [owner, a, b] at v3; the owner kicks `b` and
        // gossips v4 without it. Union semantics could never drop `b`.
        let dev = memory();
        dev.inner.lock().owner = Some("owner".into());
        assert!(dev.merge_fleet("a", "owner", &roster("k", 3, &["owner", "a", "b"])));
        assert_eq!(dev.fleet().unwrap().members.len(), 3);

        assert!(dev.merge_fleet("a", "owner", &roster("k", 4, &["owner", "a"])));
        let f = dev.fleet().unwrap();
        assert_eq!(f.version, 4);
        assert_eq!(f.members.len(), 2);
        assert!(!f.members.iter().any(|m| m.device.as_str() == "b"));

        // Stale gossip (the kicked copy echoing back at v3) is ignored.
        assert!(!dev.merge_fleet("a", "owner", &roster("k", 3, &["owner", "a", "b"])));
        assert_eq!(dev.fleet().unwrap().members.len(), 2);
    }

    #[test]
    fn a_newer_roster_without_us_means_we_were_kicked() {
        let dev = memory();
        dev.inner.lock().owner = Some("owner".into());
        assert!(dev.merge_fleet("b", "owner", &roster("k", 3, &["owner", "a", "b"])));
        // v4 arrives without us → drop the fleet entirely (and report it as
        // structural so the UI refreshes).
        assert!(dev.merge_fleet("b", "owner", &roster("k", 4, &["owner", "a"])));
        assert!(dev.fleet().is_none());
    }

    #[test]
    fn leaving_returns_the_minus_self_roster_and_clears_local_state() {
        let dev = memory();
        dev.inner.lock().owner = Some("owner".into());
        assert!(dev.merge_fleet("a", "owner", &roster("k", 3, &["owner", "a"])));

        let out = dev.leave_fleet("a-AB12C").expect("was a member");
        assert_eq!(out.version, 4, "bumped so the leave out-ranks v3 copies");
        assert!(!out.members.iter().any(|m| m.device.as_str() == "a"));
        assert!(dev.fleet().is_none());
        // Not in a fleet any more → leaving again is a no-op.
        assert!(dev.leave_fleet("a").is_none());
    }

    #[test]
    fn kicking_needs_membership_and_skips_self() {
        let dev = memory();
        dev.inner.lock().owner = Some("owner".into());
        assert!(dev.merge_fleet("a", "owner", &roster("k", 3, &["owner", "a", "b"])));

        let out = dev.kick_member("a", "b").expect("member may kick");
        assert_eq!(out.version, 4);
        assert!(!out.members.iter().any(|m| m.device.as_str() == "b"));

        // Kicking yourself is Leave's job.
        assert!(dev.kick_member("a", "a-AB12C").is_err());
        // A device that's not in the roster can't be kicked.
        assert!(dev.kick_member("a", "stranger").is_err());

        // And a non-member can't kick at all — "you can't kick others from
        // fleets you aren't in".
        let outsider = memory();
        outsider.inner.lock().fleet_key = Some("k".into());
        outsider.inner.lock().fleet_members.push(OwnedMember {
            device: "owner".into(),
            label: "Owner".into(),
        });
        assert!(outsider.kick_member("not-a-member", "owner").is_err());
    }

    #[test]
    fn sanitize_drops_incoherent_fleets_and_keeps_real_ones() {
        let member = |d: &str| OwnedMember {
            device: d.into(),
            label: d.into(),
        };

        // Residue: a roster this device isn't in at all (pre-fix bystander
        // adoption, or an identity re-mint on our side) → dropped.
        let dev = memory();
        dev.inner.lock().fleet_key = Some("k-old".into());
        dev.inner.lock().fleet_members.push(member("somebody-else"));
        assert!(dev.sanitize_fleet("me"));
        assert!(dev.fleet().is_none());
        // Idempotent once clean.
        assert!(!dev.sanitize_fleet("me"));

        // Residue: owned by a *new* owner, holding the roster of an old life
        // (we're listed, the new owner isn't) → dropped. This is the exact
        // "claimed, but the fleet never appears on the device" stale state.
        let dev = memory();
        dev.inner.lock().owner = Some("new-owner".into());
        dev.inner.lock().fleet_key = Some("k-old".into());
        dev.inner.lock().fleet_members.push(member("me"));
        dev.inner.lock().fleet_members.push(member("old-owner"));
        assert!(dev.sanitize_fleet("me"));
        assert!(dev.fleet().is_none());

        // Healthy claimed device: we and our owner are both listed → kept.
        let dev = memory();
        dev.inner.lock().owner = Some("owner".into());
        dev.inner.lock().fleet_key = Some("k".into());
        dev.inner.lock().fleet_members.push(member("owner"));
        dev.inner.lock().fleet_members.push(member("me-AB12C"));
        assert!(!dev.sanitize_fleet("me")); // display vs bare form collapses
        assert!(dev.fleet().is_some());

        // Healthy owner machine: unowned itself, member of its own fleet → kept.
        let dev = memory();
        dev.inner.lock().fleet_key = Some("k".into());
        dev.inner.lock().fleet_members.push(member("me"));
        assert!(!dev.sanitize_fleet("me-AB12C"));
        assert!(dev.fleet().is_some());
    }

    #[test]
    fn release_clears_owner_and_fleet_together() {
        let dev = memory();
        dev.inner.lock().claim_mode = true;
        assert!(dev.try_accept_claim("owner"));
        dev.ensure_fleet_key();
        dev.upsert_member("owner", "Owner");
        assert!(dev.fleet().is_some());

        assert!(dev.set_owner(None));
        assert_eq!(dev.owner(), None);
        assert!(
            dev.fleet().is_none(),
            "membership follows ownership — a released device leaves the fleet"
        );
    }

    #[test]
    fn merge_rejects_unsolicited_roster_from_a_stranger() {
        // Attack: a keyless device must not adopt a fleet just because a
        // stranger gossips a roster that lists it — that is how a peer would
        // write itself in to earn owner/fleet-only control (shell, files,
        // input). Only the recorded owner can hand down the first roster.
        let dev = memory();
        dev.inner.lock().owner = Some("my-owner".into());
        let injected = roster("attacker-key", 1, &["my-device", "attacker"]);
        assert!(
            !dev.merge_fleet("my-device", "attacker", &injected),
            "a non-owner can't seed our fleet"
        );
        assert!(dev.fleet().is_none(), "nothing adopted");

        // The same shape from the real owner (who hands it down right after
        // the authenticated claim) is accepted.
        let legit = roster("owner-key", 1, &["my-owner", "my-device"]);
        assert!(dev.merge_fleet("my-device", "my-owner", &legit));
        assert!(dev.fleet().is_some());
    }

    #[test]
    fn merge_rejects_a_key_holder_who_is_not_a_member() {
        // Even with the correct (broadcast-observable) key, a peer that is not
        // already our owner or a member cannot rewrite membership.
        let dev = memory();
        dev.inner.lock().owner = Some("owner".into());
        assert!(dev.merge_fleet("me", "owner", &roster("k", 2, &["owner", "me"])));
        assert_eq!(dev.fleet().unwrap().members.len(), 2);

        let poisoned = roster("k", 3, &["owner", "me", "attacker"]);
        assert!(
            !dev.merge_fleet("me", "attacker", &poisoned),
            "knowing the key is not membership"
        );
        assert!(
            !dev
                .fleet()
                .unwrap()
                .members
                .iter()
                .any(|m| m.device.as_str() == "attacker"),
            "attacker never entered the roster"
        );

        // The owner making the same change is honoured.
        assert!(dev.merge_fleet("me", "owner", &poisoned));
        assert!(dev
            .fleet()
            .unwrap()
            .members
            .iter()
            .any(|m| m.device.as_str() == "attacker"));
    }

    #[test]
    fn an_existing_member_may_propagate_roster_changes() {
        // Convergence still works once the owner has introduced a member: that
        // member's later gossip is accepted (owner-or-member authority), so
        // peers heal without routing every change through the owner.
        let dev = memory();
        dev.inner.lock().owner = Some("owner".into());
        assert!(dev.merge_fleet("me", "owner", &roster("k", 2, &["owner", "me", "peer"])));
        assert!(dev.merge_fleet("me", "peer", &roster("k", 3, &["owner", "me", "peer", "peer2"])));
        assert_eq!(dev.fleet().unwrap().members.len(), 4);
    }
}
