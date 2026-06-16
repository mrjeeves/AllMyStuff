//! Shared CEC conventions — the few string shapes the app backend, the agent
//! binary, the reference server, and the GUI all have to agree on. Keeping
//! them in one place (and mirroring them in `gui/src/cec.ts`) means a help
//! room minted on one side is recognised on the other.

use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// The prefix every per-customer CEC network id carries. A mesh `network_id`
/// must match `[a-z0-9_-]+`, so this — and the hex hash after it — is
/// deliberately lowercase even though the product calls it "CEC-Customer".
pub const CEC_NETWORK_PREFIX: &str = "cec-customer-";

/// The label the app shows for the customer's CEC network and for the single
/// service node on it.
pub const CEC_NETWORK_LABEL: &str = "CEC";
pub const CEC_SERVICE_LABEL: &str = "CEC Service";

/// The marker segment in a help room's nonce. A CEC help room id reads
/// `room:{host}:cec-{nonce}` — still an ordinary, on-the-wire-valid room id,
/// but recognisable as a Concierge session by either side.
pub const CEC_ROOM_MARKER: &str = "cec-";

/// Derive the stable, isolated network id for a customer account. The hash is
/// opaque (no email or account id leaks onto the mesh) and stable (the same
/// account always lands on the same network), so a customer's devices and the
/// CEC Service node always rendezvous in the same private place.
pub fn customer_network_id(account_id: &str) -> String {
    format!("{CEC_NETWORK_PREFIX}{}", short_hash(account_id))
}

/// Whether a network id is a CEC customer network.
pub fn is_cec_network(network_id: &str) -> bool {
    network_id.starts_with(CEC_NETWORK_PREFIX)
}

/// A 16-hex-char (64-bit) hash of the input — enough to be collision-free
/// across any plausible customer base, short enough to read.
pub fn short_hash(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    hex::encode(&digest[..8])
}

/// Mint a help room id hosted by `host` (a bare pubkey). The `:cec-` marker
/// lets [`is_help_room`] recognise it later without any side state.
pub fn help_room_id(host: &str) -> String {
    format!("room:{host}:{CEC_ROOM_MARKER}{}", new_nonce())
}

/// Whether a room id is a CEC help room (minted by [`help_room_id`]).
pub fn is_help_room(room_id: &str) -> bool {
    // room:{host}:cec-{nonce} — the nonce segment starts with the marker.
    room_id
        .rsplit_once(':')
        .map(|(_, nonce)| nonce.starts_with(CEC_ROOM_MARKER))
        .unwrap_or(false)
}

/// A short, process-unique nonce: time millis mixed with a monotonic counter,
/// base36-ish. Not a secret — just needs to not collide within a host.
pub fn new_nonce() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    // 11-char base36 of (millis<<20 ^ counter) — plenty of room, no deps.
    base36(millis.wrapping_shl(20) ^ n)
}

fn base36(mut v: u64) -> String {
    const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if v == 0 {
        return "0".into();
    }
    let mut buf = Vec::new();
    while v > 0 {
        buf.push(ALPHABET[(v % 36) as usize]);
        v /= 36;
    }
    buf.reverse();
    String::from_utf8(buf).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn customer_network_id_is_stable_and_lowercase() {
        let a = customer_network_id("acct_123");
        let b = customer_network_id("acct_123");
        assert_eq!(a, b);
        assert!(a.starts_with(CEC_NETWORK_PREFIX));
        assert!(is_cec_network(&a));
        assert!(a
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
        assert_ne!(a, customer_network_id("acct_456"));
    }

    #[test]
    fn non_cec_network_is_not_misread() {
        assert!(!is_cec_network("home"));
        assert!(!is_cec_network("net_abc123"));
    }

    #[test]
    fn help_room_ids_are_recognisable_and_unique() {
        let host = "abckeypubkey";
        let r1 = help_room_id(host);
        let r2 = help_room_id(host);
        assert!(is_help_room(&r1));
        assert!(is_help_room(&r2));
        assert_ne!(r1, r2, "nonce must differ");
        assert!(r1.starts_with(&format!("room:{host}:")));
        // A normal room id is not mistaken for a help room.
        assert!(!is_help_room(&format!("room:{host}:plain123")));
    }
}
