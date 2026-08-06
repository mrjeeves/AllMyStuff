//! The **Support ID** — the short number a customer reads to a technician.
//!
//! It is a deterministic function of the device's public key, so a technician
//! never needs a directory or a central server: their app sees a peer's
//! `device_id` at the signaling layer (before any connection) and computes
//! that peer's Support ID locally to match the number the customer gave.
//!
//! Format: **9 decimal digits** (numbers only — nothing to spell out or misread
//! over the phone), shown grouped as `NNN NNN NNN`. Derived by reducing the
//! first 64 bits of `SHA-256(device_id_bytes)` modulo 1e9. A billion values —
//! collisions are irrelevant because the Support ID is only a *lookup hint*: the
//! actual connection is still mutually authenticated by ed25519 at the mesh
//! layer, and the customer confirms the technician by name and by the 6-digit
//! verification code before approving.

use sha2::{Digest, Sha256};

/// Number of digits in a [`SupportId`].
pub const SUPPORT_ID_LEN: usize = 9;

/// A customer's Support ID — a 9-digit numeric code derived from their device
/// id. Cheap to clone; compares digit-for-digit via [`SupportId::matches`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SupportId(String);

impl SupportId {
    /// Derive the Support ID for a device from its `device_id` (the base32
    /// ed25519 public key MyOwnMesh puts on the wire). Any stable string id
    /// works; the same input always yields the same Support ID on every
    /// platform.
    pub fn from_device(device_id: &str) -> Self {
        SupportId(support_id_from_device(device_id))
    }

    /// The raw 9-digit code, e.g. `"123456789"`.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Grouped for display, e.g. `"123 456 789"`.
    pub fn grouped(&self) -> String {
        format_support_id(&self.0)
    }

    /// Whether a user-typed string names this Support ID. Tolerant of the
    /// grouping spaces (or dashes) and of read-alikes a customer might voice —
    /// "oh" for zero, "el"/"eye" for one — so `123-456-789`, `123 456 789`, and
    /// `123456789` all match.
    pub fn matches(&self, input: &str) -> bool {
        normalize_input(input) == self.0
    }
}

impl std::fmt::Display for SupportId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The bare pubkey part of a device id: strips a trailing `-XXXXX` display
/// suffix (dash + exactly 5 alphanumerics) if present. A device id reaches us
/// in two forms — bare on the wire (`pubkey`), suffixed everywhere a human
/// might see it (`pubkey-AB12C`) — and a Support ID derived from one form
/// would never match one derived from the other: the customer would host a
/// room no beacon points at. Every device-id derivation canonicalises through
/// this. Matches MyOwnMesh's `signing::pubkey_part` and consent's.
pub fn device_pubkey(device_id: &str) -> &str {
    if let Some((head, tail)) = device_id.rsplit_once('-') {
        if tail.len() == 5 && tail.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return head;
        }
    }
    device_id
}

/// Derive the 9-digit Support ID string for a device id, in either bare or
/// display-suffixed form — both yield the same ID. See [`SupportId`].
pub fn support_id_from_device(device_id: &str) -> String {
    support_id_from_string(device_pubkey(device_id))
}

/// Reduce an arbitrary string to 9 decimal digits — the raw hash with **no**
/// device-id canonicalisation. For inputs that aren't device ids (e.g. the
/// session verification code); device ids go through
/// [`support_id_from_device`].
pub fn support_id_from_string(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    // First 8 bytes = 64 bits, reduced modulo 1e9 to a 9-digit number. 2^64 is
    // ~1.8e10 × 1e9, so the modulo bias is negligible.
    let mut acc: u64 = 0;
    for &b in &digest[..8] {
        acc = (acc << 8) | u64::from(b);
    }
    let n = acc % 1_000_000_000;
    format!("{n:09}")
}

/// Group the digits for readability: `123456789` → `123 456 789`. Purely
/// cosmetic; [`normalize_input`] strips the spaces back out.
pub fn format_support_id(code: &str) -> String {
    let digits: String = code.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() == SUPPORT_ID_LEN {
        format!("{} {} {}", &digits[0..3], &digits[3..6], &digits[6..9])
    } else {
        code.to_string()
    }
}

/// Canonicalise user input for matching: keep only the digits (so spaces,
/// dashes, and pasted labels are tolerated), folding the read-alikes a customer
/// might voice — `O` → `0`, `I`/`L` → `1` — so a spoken "oh" or "eye" still
/// lands on the right number. Everything else is dropped.
pub fn normalize_input(input: &str) -> String {
    input
        .chars()
        .filter_map(|c| match c.to_ascii_uppercase() {
            'O' => Some('0'),
            'I' | 'L' => Some('1'),
            c @ '0'..='9' => Some(c),
            _ => None,
        })
        .collect()
}

/// Derive the private support-session room from a Support ID. The shared
/// support area is discovery-only; actual data sessions ride this room so a
/// technician can connect only to the customer whose number they were given.
pub fn network_id_for_number(number: &str) -> String {
    format!("{}{}", crate::CEC_NETWORK_PREFIX, normalize_input(number))
}

/// The private support-session room belonging to `device_id`.
pub fn network_id_for_device(device_id: &str) -> String {
    network_id_for_number(&support_id_from_device(device_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_deterministic_and_well_formed() {
        let dev = "k3m2pq7xabcdefghijklmnopqrstuvwxyz234567abcdefghijkl";
        let a = support_id_from_device(dev);
        let b = support_id_from_device(dev);
        assert_eq!(a, b, "same device -> same id");
        assert_eq!(a.len(), SUPPORT_ID_LEN);
        assert!(
            a.bytes().all(|c| c.is_ascii_digit()),
            "id {a} is digits only"
        );
    }

    #[test]
    fn different_devices_differ() {
        assert_ne!(
            support_id_from_device("k3m2pq7xabcdefghijklmnopqrstuvwxyz234567abcdefghijkl"),
            support_id_from_device("zzz2pq7xabcdefghijklmnopqrstuvwxyz234567abcdefghijkl")
        );
    }

    #[test]
    fn suffixed_and_bare_forms_agree() {
        // The wire sees the bare pubkey; humans (and local identity) see the
        // display-suffixed form. Both must land on the same Support ID, or a
        // phoned-in number wouldn't resolve to the device the beacon named.
        let bare = "dqbx4vwtyegzh47nsssg2zrqhev576jp7tkspvwr3mf4jk5wmmoa";
        let suffixed = format!("{bare}-0A307");
        assert_eq!(
            support_id_from_device(bare),
            support_id_from_device(&suffixed)
        );
        assert_eq!(
            network_id_for_device(bare),
            network_id_for_device(&suffixed)
        );
        // But a dash tail that isn't a 5-char display suffix is part of the
        // id and must NOT be stripped.
        assert_ne!(
            support_id_from_device("device-alphabet"),
            support_id_from_device("device")
        );
        // And the raw string hash never canonicalises.
        assert_ne!(
            support_id_from_string(&suffixed),
            support_id_from_string(bare)
        );
    }

    #[test]
    fn grouping_round_trips_through_normalize() {
        let id = SupportId::from_device("some-device-id");
        let grouped = id.grouped();
        assert!(grouped.contains(' '), "grouped {grouped} has spaces");
        assert_eq!(normalize_input(&grouped), id.as_str());
    }

    #[test]
    fn matches_is_tolerant() {
        let id = SupportId::from_device("device-alpha");
        let raw = id.as_str().to_string();
        assert!(id.matches(&raw));
        assert!(id.matches(&format!("  {}  ", id.grouped())));
        // A dash-grouped form the technician might type still matches.
        let dashed = format!("{}-{}-{}", &raw[0..3], &raw[3..6], &raw[6..9]);
        assert!(id.matches(&dashed));
        assert!(!id.matches("000000000"));
    }

    #[test]
    fn normalize_keeps_digits_and_folds_read_alikes() {
        // A customer who reads O for zero and l/I for one still matches.
        assert_eq!(normalize_input("O1lI"), "0111");
        // Spaces, dashes, and stray letters are dropped.
        assert_eq!(normalize_input("123 456-789 abc"), "123456789");
    }

    #[test]
    fn session_room_prefix_stays_frozen() {
        // Session rooms match `CEC_NETWORK_PREFIX` + 9 digits; the prefix must
        // never drift or two builds will choose different rooms for one customer.
        assert_eq!(crate::CEC_NETWORK_PREFIX, "cec-");
        assert_eq!(network_id_for_number("123 456 789"), "cec-123456789");
    }
}
