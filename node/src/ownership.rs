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

use std::path::{Path, PathBuf};

use allmystuff_graph::NodeId;
use allmystuff_protocol::OwnedMember;
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
    /// Whether this device **minted** the fleet key (vs. having it handed down
    /// when adopted). Only a minter founds the fleet's closed-network genesis;
    /// a device merely holding an adopted key must never self-elect a *parallel*
    /// genesis. `Option` so a legacy file (no field) migrates on load — an
    /// existing un-owned key-holder is assumed to be its own minter.
    #[serde(default)]
    minted: Option<bool>,
    /// The fleet's display name ("Casey") — cosmetic, empty when unnamed.
    /// Handed down with the fleet key when this device is adopted; the
    /// owner's copy is authoritative.
    #[serde(default)]
    fleet_name: String,
    /// A local change counter, bumped whenever this device's view of the
    /// fleet mutates (claim, kick, rename, adopt). Cosmetic — surfaced to
    /// the GUI as the roster "version" so newer renders win. No longer a
    /// gossip convergence clock (the closed network's signed roster is the
    /// authority now).
    #[serde(default)]
    fleet_version: u64,
    /// The owner's local record of the devices it has claimed into its
    /// fleet, in canonical-pubkey form. The **owner** keeps this so it can
    /// (re-)admit every member into the fleet's closed-network signed
    /// roster on startup; a non-owner member leaves it empty and reads
    /// membership from the signed roster itself. Not gossiped.
    #[serde(default)]
    fleet_members: Vec<OwnedMember>,
    /// Whether this device participates in claiming **over the public
    /// mesh** (claim-code rendezvous over remote signaling). Off by
    /// default — claiming is LAN-local unless deliberately enabled.
    /// STRICTLY DEVICE-LOCAL policy: set only here (or via the
    /// `ALLMYSTUFF_PUBLIC_CLAIMS` env), never synced from the fleet and
    /// never settable by a remote peer — a remote system must not be
    /// able to open a device to public claiming.
    #[serde(default)]
    public_claims: bool,
    /// The device's current claim code (claimee side): the random
    /// rendezvous secret behind its `amsclaim-<code>` network while it
    /// sits claimable with public claims enabled. Persisted so the code
    /// an operator read off the device survives a restart; rotated
    /// after every successful claim.
    #[serde(default)]
    claim_code: Option<String>,
}

/// Live state behind one lock, so a claim's check-and-set is atomic.
#[derive(Debug, Default)]
struct Inner {
    owner: Option<String>,
    claim_mode: bool,
    fleet_key: Option<String>,
    /// True iff this device minted `fleet_key` (gates founding — see
    /// [`Ownership::is_fleet_founder`]).
    minted: bool,
    fleet_name: String,
    fleet_version: u64,
    fleet_members: Vec<OwnedMember>,
    public_claims: bool,
    claim_code: Option<String>,
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
        // Missing file → blank record; corrupt file → quarantined aside +
        // blank, loudly (never silently: a blank record here means "unowned",
        // and an owner forgotten without a trace is how a device ends up
        // claimable by whoever's fastest).
        let persisted: Persisted = path
            .as_ref()
            .map(|p| crate::persist::load_json(p))
            .unwrap_or_default();
        // Resolve `minted`, migrating a legacy file (no field): an existing
        // un-owned key-holder is assumed to be its own minter, so the real
        // founder keeps founding, while any future *adopted* key (which sets the
        // flag false explicitly) never self-elects a parallel genesis.
        let minted = persisted
            .minted
            .unwrap_or_else(|| persisted.owner.is_none() && persisted.fleet_key.is_some());
        let inner = Inner {
            claim_mode: persisted.owner.is_none() && env_claim_flag(),
            owner: persisted.owner,
            fleet_key: persisted.fleet_key,
            minted,
            fleet_name: persisted.fleet_name,
            fleet_version: persisted.fleet_version,
            fleet_members: persisted.fleet_members,
            public_claims: persisted.public_claims,
            claim_code: persisted.claim_code,
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
    /// which a claim is accepted. A device already **in a fleet** — claimed
    /// (it has an owner) *or* the founder of one (it holds a fleet key) — is
    /// never claimable: a claimed device can't be re-adopted, and a fleet
    /// owner offering itself for adoption would be conscripted into another
    /// fleet while still owning its own.
    pub fn claimable(&self) -> bool {
        let i = self.inner.lock();
        i.owner.is_none() && i.claim_mode && i.fleet_key.is_none()
    }

    /// Record (or clear) the owner. Recording one ends claim mode — an owned
    /// device is never claimable until its owner releases it. Clearing the
    /// owner (a release) also leaves the fleet: membership follows ownership,
    /// so the local fleet credential is dropped. The caller is responsible
    /// for tearing the device out of the fleet's closed network (see
    /// `Mesh`'s release/kick handling) — this only clears the durable record.
    /// Returns whether the durable write succeeded.
    pub fn set_owner(&self, owner: Option<String>) -> bool {
        let mut i = self.inner.lock();
        i.owner = owner;
        if i.owner.is_some() {
            i.claim_mode = false;
        } else {
            i.fleet_key = None;
            i.fleet_name.clear();
            i.fleet_version = 0;
            i.fleet_members.clear();
        }
        persist(&self.path, &i)
    }

    /// Turn claim mode on or off at runtime. Only meaningful for a device
    /// that's free to be adopted — not owned, and not already the founder of
    /// its own fleet (see [`Ownership::claimable`]).
    pub fn set_claim_mode(&self, on: bool) {
        let mut i = self.inner.lock();
        i.claim_mode = on && i.owner.is_none() && i.fleet_key.is_none();
    }

    // ---- public-mesh claiming policy (strictly device-local) -------------
    //
    // Claiming is LAN-local by default: claimable presence and inbound
    // claims ride the well-known mDNS-only claim network and nothing
    // else. "Claims over the public mesh" — the claim-code rendezvous
    // over remote signaling — must be deliberately enabled **on this
    // device** (this setting, or `ALLMYSTUFF_PUBLIC_CLAIMS` for a
    // headless box). It is never synced from a fleet and never settable
    // by a remote peer: no remote system may open a device to public
    // claiming.

    /// The persisted device-local public-claims setting.
    pub fn public_claims(&self) -> bool {
        self.inner.lock().public_claims
    }

    /// Whether public-mesh claiming is allowed on this device right
    /// now: the persisted setting, or the deploy-time env escape hatch
    /// for headless claimees.
    pub fn public_claims_allowed(&self) -> bool {
        self.public_claims() || env_public_claims_flag()
    }

    /// Flip the device-local public-claims setting. Bumps the fleet
    /// version so GUI snapshots refresh. Returns whether the durable
    /// write succeeded.
    pub fn set_public_claims(&self, on: bool) -> bool {
        let mut i = self.inner.lock();
        i.public_claims = on;
        i.fleet_version = i.fleet_version.saturating_add(1);
        persist(&self.path, &i)
    }

    /// The device's current claim code, if one has been minted.
    pub fn claim_code(&self) -> Option<String> {
        self.inner.lock().claim_code.clone()
    }

    /// The claim code, minting (and persisting) a fresh one if absent.
    /// The code is the rendezvous secret behind the device's
    /// `amsclaim-<code>` network while it sits claimable with public
    /// claims enabled.
    pub fn ensure_claim_code(&self) -> String {
        let mut i = self.inner.lock();
        if let Some(code) = &i.claim_code {
            return code.clone();
        }
        let code = new_claim_code();
        i.claim_code = Some(code.clone());
        let _ = persist(&self.path, &i);
        code
    }

    /// Discard the claim code so the next [`Self::ensure_claim_code`]
    /// mints a fresh one. Called after a successful claim — a code that
    /// has admitted someone is spent.
    pub fn rotate_claim_code(&self) {
        let mut i = self.inner.lock();
        if i.claim_code.take().is_some() {
            let _ = persist(&self.path, &i);
        }
    }

    /// Accept a claim from `claimer` — but only if the device is currently
    /// claimable **and** the new owner can be durably recorded. Both the
    /// check and the set happen under one lock so a claim can't race another
    /// or be acknowledged without being persisted. Returns whether it took.
    ///
    /// Accepting also **resets any fleet state**: this device is joining its
    /// new owner's fleet from scratch. A key left over from an earlier
    /// ownership would derive a different (stale) closed network; the owner
    /// hands down the real fleet key right after the claim
    /// ([`OwnershipControl::FleetKey`] → [`Ownership::adopt_fleet_key`]).
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

    // ---- owned fleet (a closed MyOwnMesh network) -----------------------
    //
    // Claiming a device links the two machines under a shared **fleet key**.
    // The owner mints the key on its first claim and hands it down to each
    // device it adopts ([`Ownership::adopt_fleet_key`]). Both sides derive
    // the same closed-network id from the key; the owner founds that network
    // (electing itself Owner) and admits members, and its **signed roster**
    // is the authority for membership and control. There is no gossiped
    // `OwnedRoster` any more — the key handoff plus the signed roster replace
    // it entirely.

    /// The shared fleet key this device holds, if it belongs to a fleet.
    pub fn fleet_key(&self) -> Option<String> {
        self.inner.lock().fleet_key.clone()
    }

    /// The local fleet change counter (surfaced to the GUI as the roster
    /// version). Cosmetic; not a convergence clock.
    pub fn fleet_version(&self) -> u64 {
        self.inner.lock().fleet_version
    }

    /// Adopt a fleet key handed down by this device's owner right after a
    /// claim. Sets the key (and the fleet name, if we don't already have
    /// one) so this device derives — and joins — the same closed network.
    /// Ignored if we already hold this exact key. Returns whether anything
    /// changed.
    pub fn adopt_fleet_key(&self, key: &str, name: &str) -> bool {
        if key.is_empty() {
            return false;
        }
        let mut i = self.inner.lock();
        let mut changed = false;
        if i.fleet_key.as_deref() != Some(key) {
            i.fleet_key = Some(key.to_string());
            // Handed down, not minted — an adopted-key device joins the
            // founder's fleet and must never self-elect a parallel genesis.
            i.minted = false;
            changed = true;
        }
        if !name.is_empty() && i.fleet_name != name {
            i.fleet_name = name.to_string();
            changed = true;
        }
        if changed {
            i.fleet_version += 1;
            persist(&self.path, &i);
        }
        changed
    }

    /// The closed MyOwnMesh network that backs this fleet, derived
    /// deterministically from the fleet key so every co-owned device computes
    /// the **same** id without being told it — that is what makes the move to
    /// closed-network governance self-migrating. `None` when not in a fleet.
    ///
    /// This network's signed roster is the authority for who may control this
    /// device (see `Mesh::sender_may_control`).
    pub fn fleet_network_id(&self) -> Option<String> {
        let i = self.inner.lock();
        i.fleet_key.as_deref().map(derive_fleet_network_id)
    }

    /// The fleet display name, if any (for naming the closed network).
    pub fn fleet_name(&self) -> String {
        self.inner.lock().fleet_name.clone()
    }

    /// Whether this device is the fleet's **owner** — it minted the key and is
    /// owned by no-one — i.e. the device responsible for founding the fleet's
    /// closed network and admitting members into its signed roster.
    pub fn is_fleet_owner(&self) -> bool {
        let i = self.inner.lock();
        i.owner.is_none() && i.fleet_key.is_some()
    }

    /// Whether this device may **found** the fleet's closed-network genesis: it
    /// owns the fleet (un-owned key-holder) *and* it **minted** the key. A
    /// device that merely adopted a key is a fleet owner structurally but must
    /// not self-elect a *second, parallel* genesis — that's the split-brain the
    /// engine then (correctly) refuses to merge. Distinct fleets stay distinct;
    /// consolidating two is a deliberate leave-and-rejoin, never an auto-merge.
    pub fn is_fleet_founder(&self) -> bool {
        let i = self.inner.lock();
        i.owner.is_none() && i.fleet_key.is_some() && i.minted
    }

    /// Canonical member device-ids of the fleet — for the owner to admit into
    /// the closed-network roster. Empty when not in a fleet.
    pub fn fleet_member_ids(&self) -> Vec<String> {
        self.inner
            .lock()
            .fleet_members
            .iter()
            .map(|m| m.device.as_str().to_string())
            .collect()
    }

    /// Allocation-free membership probe for per-event gates — the input
    /// path runs its owner/fleet check on every mouse move, and
    /// [`Self::fleet_member_ids`] clones the whole roster per call (fine
    /// for a roster op, not for 60 Hz input). Same lock, same iteration,
    /// no `Vec`.
    pub fn any_fleet_member(&self, pred: impl Fn(&str) -> bool) -> bool {
        self.inner
            .lock()
            .fleet_members
            .iter()
            .any(|m| pred(m.device.as_str()))
    }

    /// The owner's local member records (device + label). The owner's durable,
    /// persisted view of who's in its fleet — kept consistent with the signed
    /// roster (a left/evicted device is dropped from both), so it's safe to
    /// fold into the roster shown to the GUI to cover a startup lag or a
    /// transient signed-roster read failure. Empty for a non-owner member.
    pub fn fleet_members(&self) -> Vec<OwnedMember> {
        self.inner.lock().fleet_members.clone()
    }

    /// Make sure this device has a fleet key, minting a fresh one the first
    /// time (e.g. when it claims its first device). Returns the key.
    pub fn ensure_fleet_key(&self) -> String {
        let mut i = self.inner.lock();
        if i.fleet_key.is_none() {
            i.fleet_key = Some(new_fleet_key());
            // We minted this key, so this device is the fleet's founder — the
            // only device that may self-elect the closed-network genesis.
            i.minted = true;
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

    /// Whether this device is in a fleet **at all** — the single membership
    /// predicate every other check derives from. True when it holds a fleet key
    /// (a founder, or an adopted member) *or* it's been claimed (has an owner).
    /// The `owner` arm is what makes an owned-but-keyless device — claimed, but
    /// still awaiting its owner's key handoff — count as in a fleet, so the
    /// drawer, the settings pane and `leave` all agree instead of one saying
    /// "in a fleet" while another insists it isn't.
    pub fn in_fleet(&self) -> bool {
        let i = self.inner.lock();
        i.owner.is_some() || i.fleet_key.is_some()
    }

    /// Leave the fleet this device belongs to, clearing **all** local
    /// fleet/ownership state — owner, key, name, members — in one atomic step.
    /// Returns the derived closed-network id to tear out of (`Some`) when this
    /// device held a key, or `None` when it didn't (an owned-but-keyless member
    /// that never joined the network — there's nothing to `NetworkRemove`, but
    /// it has still left). `Err` only when there was nothing to leave: no owner
    /// and no key. Clearing the owner here is deliberate — membership follows
    /// ownership, so leaving releases this device to re-advertise unowned.
    pub fn leave_fleet(&self) -> Result<Option<String>, &'static str> {
        let mut i = self.inner.lock();
        if i.owner.is_none() && i.fleet_key.is_none() {
            return Err("this device isn't in a fleet");
        }
        let network = i.fleet_key.take().map(|k| derive_fleet_network_id(&k));
        i.owner = None;
        i.fleet_name.clear();
        i.fleet_version = 0;
        i.fleet_members.clear();
        persist(&self.path, &i);
        Ok(network)
    }

    /// Forget `device` from the owner's local member record (the re-admit
    /// list). The propagating removal itself is a closed-network **Evict**
    /// the caller drives; this just keeps the local record honest so the
    /// kicked device isn't re-admitted on the next `ensure`. Returns the
    /// fleet's closed-network id, or an error if this device isn't in a
    /// fleet.
    pub fn kick_member(&self, device: &str) -> Result<String, String> {
        let mut i = self.inner.lock();
        let Some(key) = i.fleet_key.clone() else {
            return Err("this device isn't in a fleet".into());
        };
        let canon = pubkey_part(device).to_string();
        let before = i.fleet_members.len();
        i.fleet_members
            .retain(|m| pubkey_part(m.device.as_str()) != canon);
        if i.fleet_members.len() != before {
            i.fleet_version += 1;
            persist(&self.path, &i);
        }
        Ok(derive_fleet_network_id(&key))
    }

    /// Name (or rename) the fleet locally. Bumps the version so the GUI
    /// refreshes; the closed network's label is updated by the caller. You
    /// can't name a fleet this device isn't in.
    pub fn set_fleet_name(&self, name: &str) -> Result<(), String> {
        let name = name.trim();
        let mut i = self.inner.lock();
        if i.fleet_key.is_none() {
            return Err("this device isn't in a fleet".into());
        }
        if i.fleet_name != name {
            i.fleet_name = name.to_string();
            i.fleet_version += 1;
            persist(&self.path, &i);
        }
        Ok(())
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
                // Stamped on first add only. A later upsert is a label refresh,
                // not a fresh claim, so it must not move this — the stamp is
                // what tells `ensure_fleet_network` whether a signed eviction
                // predates the claim, and a gossip-driven label touch would
                // otherwise keep pushing it past every tombstone.
                // An unset clock stamps this low, which reads as older than any
                // eviction — the conservative direction (prune), not the one
                // that resurrects a device.
                claimed_at: Some(crate::cec::now_secs()),
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

/// Adjective + name word-lists for a fleet's deterministic network name.
///
/// FROZEN once shipped: changing either list (or the derivation below) changes
/// the id a given key derives, which would strand existing fleets on the id
/// they already converged on. Add to the *end* only if ever extended.
const FLEET_ADJECTIVES: &[&str] = &[
    "amber", "ancient", "autumn", "bold", "brave", "bright", "brisk", "calm", "clever", "cobalt",
    "cosmic", "crimson", "daring", "dawn", "dusky", "eager", "elder", "ember", "fabled", "fancy",
    "fleet", "frosty", "gentle", "gilded", "golden", "hardy", "hidden", "humble", "ivory", "jolly",
    "keen", "lively", "lucky", "mellow", "merry", "mighty", "nimble", "noble", "polar", "quiet",
    "rapid", "royal", "rugged", "silent", "solar", "spry", "stout", "sunny", "swift", "tidal",
    "vivid", "wily",
];

const FLEET_NAMES: &[&str] = &[
    "ampere",
    "archimedes",
    "babbage",
    "bardeen",
    "bell",
    "bohr",
    "boyle",
    "carson",
    "curie",
    "dalton",
    "darwin",
    "dijkstra",
    "edison",
    "einstein",
    "euclid",
    "euler",
    "faraday",
    "fermi",
    "feynman",
    "franklin",
    "galileo",
    "gauss",
    "hawking",
    "heisenberg",
    "hertz",
    "hopper",
    "hubble",
    "joule",
    "kepler",
    "knuth",
    "lamarr",
    "lovelace",
    "maxwell",
    "meitner",
    "mendel",
    "morse",
    "newton",
    "noether",
    "nobel",
    "pascal",
    "pasteur",
    "planck",
    "ramanujan",
    "sagan",
    "tesla",
    "turing",
    "volta",
    "watt",
];

/// Derive the fleet's closed-network id from its key. Deterministic so every
/// co-owned device computes the identical id (self-converging migration). The
/// id is an *identifier*, not a secret (it rides in signaling), and the design
/// wants it **human-communicable** — sayable, memorable, reusable — so it reads
/// as a git-branch-style word salad (`adjective-name-suffix`, e.g.
/// `swift-mendel-q4z7a`) rather than a hash. The two words make it speakable;
/// the 5-char base36 suffix carries the entropy that keeps distinct fleets
/// apart. Lowercase alphanumerics + `-`, a valid MyOwnMesh network id.
fn derive_fleet_network_id(key: &str) -> String {
    let h1 = fnv1a64(key.as_bytes());
    // A second digest over the reversed key gives independent bits for the
    // suffix, so it doesn't track the word choice.
    let reversed: Vec<u8> = key.bytes().rev().collect();
    let h2 = fnv1a64(&reversed);
    let adjective = FLEET_ADJECTIVES[(h1 % FLEET_ADJECTIVES.len() as u64) as usize];
    // Shift before the modulo so the name doesn't correlate with the adjective.
    let name = FLEET_NAMES[((h1 >> 21) % FLEET_NAMES.len() as u64) as usize];
    format!("{adjective}-{name}-{}", base36(h2, 5))
}

/// FNV-1a, 64-bit. Stable, dependency-free, good enough for a non-secret id.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// `n` rendered as `width` lowercase base36 chars (low digit first) — the
/// readable suffix that disambiguates a derived fleet name.
fn base36(mut n: u64, width: usize) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = String::with_capacity(width);
    for _ in 0..width {
        out.push(DIGITS[(n % 36) as usize] as char);
        n /= 36;
    }
    out
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
        minted: Some(inner.minted),
        fleet_name: inner.fleet_name.clone(),
        fleet_version: inner.fleet_version,
        fleet_members: inner.fleet_members.clone(),
        public_claims: inner.public_claims,
        claim_code: inner.claim_code.clone(),
    };
    match serde_json::to_string_pretty(&persisted) {
        Ok(json) => write_private(path, json.as_bytes()),
        Err(_) => false,
    }
}

/// Write `bytes` to `path`, owner-only on Unix (mode 0600). This file holds the
/// plaintext fleet key, so a secret at rest mustn't be left world-readable
/// under the umask — the audit's AMS-08. The atomic writer creates its temp
/// file 0600 from the start and replaces the old file wholesale via rename,
/// so the key never lands in a file other local users can read *and* a power
/// cut mid-write can't truncate the ownership record (which would load back
/// as "unowned"). (A full at-rest fix wraps the key in the OS keychain; this
/// is the cheap, always-on floor.)
fn write_private(path: &Path, bytes: &[u8]) -> bool {
    crate::persist::write_atomic(path, bytes).is_ok()
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
    env_truthy("ALLMYSTUFF_CLAIMABLE")
}

/// The deploy-time public-claims escape hatch for headless claimees:
/// `ALLMYSTUFF_PUBLIC_CLAIMS` set to a truthy value. Same parsing as
/// [`env_claim_flag`]; a deployed-software setting, per the rule that
/// public claiming is only ever enabled on the device itself.
fn env_public_claims_flag() -> bool {
    env_truthy("ALLMYSTUFF_PUBLIC_CLAIMS")
}

fn env_truthy(var: &str) -> bool {
    std::env::var(var)
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

/// Mint a fresh claim code: [`allmystuff_protocol::CLAIM_CODE_BYTES`]
/// bytes of system randomness in the FROZEN base32 encoding.
fn new_claim_code() -> String {
    let mut bytes = [0u8; allmystuff_protocol::CLAIM_CODE_BYTES];
    getrandom::getrandom(&mut bytes).expect("system RNG unavailable for claim code");
    allmystuff_protocol::claim_code_from_bytes(&bytes)
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

    /// Public-mesh claiming is off unless deliberately enabled on this
    /// device — the load-bearing default.
    #[test]
    fn public_claims_defaults_off_and_flips_locally() {
        let o = memory();
        assert!(!o.public_claims());
        assert!(o.set_public_claims(true));
        assert!(o.public_claims());
        assert!(o.set_public_claims(false));
        assert!(!o.public_claims());
    }

    /// An old ownership file (no new fields) loads with the policy off
    /// and no claim code.
    #[test]
    fn legacy_persisted_file_defaults_new_fields() {
        let legacy: Persisted =
            serde_json::from_str(r#"{"owner":"boss","fleet_key":"k"}"#).expect("legacy parses");
        assert!(!legacy.public_claims);
        assert!(legacy.claim_code.is_none());
    }

    /// The claim code is stable until rotated, then re-mints fresh.
    #[test]
    fn claim_code_mints_persists_and_rotates() {
        let o = memory();
        assert!(o.claim_code().is_none());
        let first = o.ensure_claim_code();
        assert_eq!(first.len(), 26, "16 bytes of base32");
        assert_eq!(o.ensure_claim_code(), first, "stable until rotated");
        o.rotate_claim_code();
        assert!(o.claim_code().is_none());
        let second = o.ensure_claim_code();
        assert_ne!(second, first, "rotation mints a fresh secret");
    }

    /// AMS-08: the ownership file holds the plaintext fleet key, so `persist`
    /// must leave it owner-only (0600) — even when an older build left it
    /// world-readable.
    #[cfg(unix)]
    #[test]
    fn ownership_file_is_written_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!("ams-own-{}.json", std::process::id()));
        // Pre-create it world-readable to prove we tighten an existing file.
        std::fs::write(&path, b"{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        assert!(persist(&Some(path.clone()), &Inner::default()));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the fleet key at rest must be owner-only");
        let _ = std::fs::remove_file(&path);
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
    fn only_the_key_minter_is_a_founder() {
        // Minter: ensure_fleet_key mints, so this device founds the genesis.
        let minter = memory();
        minter.ensure_fleet_key();
        assert!(minter.is_fleet_owner());
        // the key-minter founds the genesis
        assert!(minter.is_fleet_founder());

        // Adopter: holds an un-owned key it was *handed*, but didn't mint — so
        // it's structurally a fleet owner, yet must never self-elect a parallel
        // genesis (the split-brain the engine then refuses to merge).
        let adopter = memory();
        assert!(adopter.adopt_fleet_key("some-handed-down-key", "Casey"));
        // un-owned key-holder => structurally a fleet owner
        assert!(adopter.is_fleet_owner());
        // ...but an adopted key must not found a second, parallel genesis
        assert!(!adopter.is_fleet_founder());
    }

    #[test]
    fn legacy_record_without_minted_is_assumed_its_own_founder() {
        // A pre-`minted` file: an un-owned key-holder with no `minted` field is
        // migrated to founder, so the real minter keeps founding after upgrade.
        let legacy: Persisted = serde_json::from_str(r#"{"fleet_key":"abc"}"#).unwrap();
        // legacy file carries no minted field
        assert_eq!(legacy.minted, None);
        let resolved = legacy
            .minted
            .unwrap_or_else(|| legacy.owner.is_none() && legacy.fleet_key.is_some());
        // an existing un-owned key-holder migrates to founder
        assert!(resolved);

        // A legacy *claimed* device (owner set) is never a founder.
        let claimed: Persisted =
            serde_json::from_str(r#"{"owner":"laptop","fleet_key":"abc"}"#).unwrap();
        let resolved_claimed = claimed
            .minted
            .unwrap_or_else(|| claimed.owner.is_none() && claimed.fleet_key.is_some());
        // a claimed device never founds
        assert!(!resolved_claimed);
    }

    #[test]
    fn upsert_stamps_the_first_add_and_a_label_refresh_never_moves_it() {
        // The stamp dates this owner's CLAIM of the device, and the eviction
        // prune in `ensure_fleet_network` compares it against the tombstone. A
        // later upsert is a label refresh driven by roster gossip, not a fresh
        // claim — if it moved the stamp, a device would drift past every
        // eviction just by being talked about.
        let mut members = Vec::new();
        assert!(upsert_member_into(&mut members, "k1", "Laptop"));
        let stamped = members[0].claimed_at.expect("first add must be stamped");

        assert!(upsert_member_into(&mut members, "k1", "My laptop"));
        assert_eq!(
            members[0].claimed_at,
            Some(stamped),
            "a label refresh moved the claim stamp"
        );
    }

    #[test]
    fn a_record_without_a_claim_stamp_still_loads() {
        // Records written before the field existed must keep loading: this is
        // the owner's fleet list, and a parse failure would drop real members.
        // `None` reads as older than any eviction, so such a device keeps the
        // previous always-prune behaviour rather than being resurrected by an
        // upgrade.
        let m: OwnedMember = serde_json::from_str(r#"{"device":"k1","label":"Laptop"}"#).unwrap();
        assert_eq!(m.claimed_at, None);
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
    fn claiming_records_members_and_handoff_adopts_the_key() {
        // Owner mints a key and records itself + a claimed device in its
        // local re-admit list. The key derives the fleet's closed network.
        let owner = memory();
        let key = owner.ensure_fleet_key();
        assert!(!key.is_empty());
        assert!(owner.upsert_member("owner-AAAAA", "Owner"));
        assert!(owner.upsert_member("nuc-BBBBB", "Spare NUC"));
        assert_eq!(owner.fleet_member_ids().len(), 2);
        let net = owner.fleet_network_id().expect("owner has a fleet network");

        // The claimed device starts blank, then adopts the key the owner
        // hands down — deriving the *same* closed-network id.
        let target = memory();
        assert!(target.adopt_fleet_key(&key, "Casey"));
        assert_eq!(target.fleet_key().as_deref(), Some(key.as_str()));
        assert_eq!(target.fleet_network_id().as_deref(), Some(net.as_str()));
        assert_eq!(target.fleet_name(), "Casey");
        // Re-adopting the same key is a no-op.
        assert!(!target.adopt_fleet_key(&key, "Casey"));
        // An empty key is rejected.
        assert!(!target.adopt_fleet_key("", "x"));
    }

    #[test]
    fn accepting_a_claim_resets_stale_fleet_state() {
        // The device holds a fleet key from an earlier life (previous owner,
        // re-minted identity, old test run). Accepting a new claim must drop
        // it — otherwise this device would derive a stale closed network and
        // never join the new owner's fleet.
        let dev = memory();
        dev.inner.lock().claim_mode = true;
        dev.inner.lock().fleet_key = Some("stale-key".into());
        dev.inner.lock().fleet_version = 9;
        dev.inner.lock().fleet_members.push(OwnedMember {
            device: "old-owner".into(),
            label: "Old".into(),
            claimed_at: None,
        });

        assert!(dev.try_accept_claim("new-owner"));
        assert!(dev.fleet_key().is_none(), "stale fleet must be gone");

        // The new owner hands down a fresh key — adopted cleanly.
        assert!(dev.adopt_fleet_key("fresh-key", ""));
        assert_eq!(dev.fleet_key().as_deref(), Some("fresh-key"));
    }

    #[test]
    fn renaming_needs_a_fleet_and_bumps_the_version() {
        let owner = memory();
        // Can't name a fleet you aren't in.
        assert!(owner.set_fleet_name("Casey").is_err());

        owner.ensure_fleet_key();
        let before = owner.fleet_version();
        owner.set_fleet_name("  Casey  ").unwrap();
        assert_eq!(owner.fleet_name(), "Casey", "name is trimmed");
        assert_eq!(owner.fleet_version(), before + 1);

        // Renaming to the same name is a no-op that doesn't bump the version.
        let v = owner.fleet_version();
        owner.set_fleet_name("Casey").unwrap();
        assert_eq!(owner.fleet_version(), v);
    }

    #[test]
    fn leaving_drops_the_credential_and_returns_the_network_id() {
        let dev = memory();
        let key = dev.ensure_fleet_key();
        let net = derive_fleet_network_id(&key);
        assert!(dev.in_fleet());

        let left = dev.leave_fleet().expect("was in a fleet");
        assert_eq!(
            left.as_deref(),
            Some(net.as_str()),
            "returns the network to tear down"
        );
        assert!(dev.fleet_key().is_none());
        assert!(!dev.in_fleet());
        // Not in a fleet any more → leaving again errors (nothing to leave).
        assert!(dev.leave_fleet().is_err());
    }

    #[test]
    fn an_owned_but_keyless_device_is_in_a_fleet_and_can_leave() {
        // A device claimed by an owner whose fleet-key handoff never landed:
        // it has an owner but no key. It's still in a fleet, and leaving must
        // succeed (clearing the owner) rather than insisting it isn't.
        let dev = memory();
        assert!(dev.set_owner(Some("desktop-AAAAA".into())));
        assert!(dev.fleet_key().is_none());
        assert!(dev.in_fleet(), "claimed without a key is still in a fleet");

        let left = dev.leave_fleet().expect("a claimed device can leave");
        assert!(left.is_none(), "no closed network was ever joined");
        assert!(dev.owner().is_none(), "leaving releases the owner");
        assert!(!dev.in_fleet());
    }

    #[test]
    fn kicking_forgets_the_member_from_the_re_admit_list() {
        let owner = memory();
        let key = owner.ensure_fleet_key();
        owner.upsert_member("owner-AAAAA", "Owner");
        owner.upsert_member("nuc-BBBBB", "Spare NUC");
        assert_eq!(owner.fleet_member_ids().len(), 2);

        let net = owner.kick_member("nuc-BBBBB").expect("in a fleet");
        assert_eq!(net, derive_fleet_network_id(&key));
        assert_eq!(owner.fleet_member_ids().len(), 1);
        assert!(!owner
            .fleet_member_ids()
            .iter()
            .any(|d| pubkey_part(d) == "nuc"));

        // Kicking when not in a fleet is an error.
        let stray = memory();
        assert!(stray.kick_member("whoever").is_err());
    }

    #[test]
    fn fleet_network_id_is_a_deterministic_word_salad() {
        let key = new_fleet_key();
        let a = derive_fleet_network_id(&key);
        // Deterministic: the same key always derives the same name — this is
        // what makes every co-owned device converge on one network.
        assert_eq!(a, derive_fleet_network_id(&key));

        // adjective-name-suffix shape, drawn from the frozen word-lists.
        let parts: Vec<&str> = a.split('-').collect();
        assert_eq!(parts.len(), 3, "{a} should be adjective-name-suffix");
        assert!(FLEET_ADJECTIVES.contains(&parts[0]), "{a}");
        assert!(FLEET_NAMES.contains(&parts[1]), "{a}");
        assert_eq!(parts[2].len(), 5, "5-char suffix in {a}");

        // A valid (lowercase) MyOwnMesh network id, and distinct keys differ.
        assert!(a
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
        assert_ne!(a, derive_fleet_network_id(&new_fleet_key()));
    }

    #[test]
    fn a_device_in_a_fleet_is_not_claimable() {
        // Fresh + claim mode on → claimable.
        let dev = memory();
        dev.set_claim_mode(true);
        assert!(dev.claimable());

        // Founding a fleet (minting a key on first claim) disables it: an
        // owner can't be conscripted into someone else's fleet.
        dev.ensure_fleet_key();
        assert!(!dev.claimable(), "a fleet owner isn't claimable");
        dev.set_claim_mode(true);
        assert!(!dev.claimable(), "and can't be toggled back on");

        // A claimed device (has an owner) isn't claimable either.
        let member = memory();
        member.inner.lock().claim_mode = true;
        assert!(member.try_accept_claim("owner"));
        assert!(!member.claimable());
    }

    #[test]
    fn release_clears_owner_and_fleet_together() {
        let dev = memory();
        dev.inner.lock().claim_mode = true;
        assert!(dev.try_accept_claim("owner"));
        dev.adopt_fleet_key("shared-key", "Casey");
        assert!(dev.fleet_key().is_some());

        assert!(dev.set_owner(None));
        assert_eq!(dev.owner(), None);
        assert!(
            dev.fleet_key().is_none(),
            "membership follows ownership — a released device leaves the fleet"
        );
    }
}
