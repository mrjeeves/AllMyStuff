//! Assemble the phone's [`NodeProfile`] — what it tells the mesh about
//! itself on the presence channel.
//!
//! A phone has no hardware scan behind it (there is no `Inventory` here), so
//! the profile is built straight from a small config plus the synthetic
//! capability set from [`crate::caps`]. The shape is identical to a desktop's
//! profile, so a peer renders a phone on the same graph with no special case.

use allmystuff_graph::NodeId;
use allmystuff_protocol::{InventorySummary, NodeProfile, PROTOCOL_VERSION};

use crate::caps::{mobile_capabilities, mobile_features, MobileScope};

/// Everything platform code knows about the phone, distilled to what the
/// presence advert needs. `boot` and `version` are supplied by the caller
/// (this crate has no clock and no build metadata of its own).
#[derive(Debug, Clone)]
pub struct MobileNodeConfig {
    /// Display name — "Chris's iPhone". Falls back to `model` if empty.
    pub label: String,
    /// The host OS string for the node card — "iOS 18.4", "Android 15".
    pub os: String,
    /// Marketing model name shown where a desktop shows its CPU — "iPhone 15
    /// Pro", "Pixel 9".
    pub model: String,
    /// Total RAM in bytes, if the platform offers it (`0` = unknown).
    pub ram_bytes: u64,
    /// How much of itself this phone exposes.
    pub scope: MobileScope,
}

impl MobileNodeConfig {
    /// A minimal viewer/controller config — just the two strings a phone
    /// always knows.
    pub fn new(label: impl Into<String>, os: impl Into<String>) -> Self {
        MobileNodeConfig {
            label: label.into(),
            os: os.into(),
            model: String::new(),
            ram_bytes: 0,
            scope: MobileScope::ViewerController,
        }
    }
}

/// Build the phone's presence profile.
///
/// * `node` is the phone's mesh id (its ed25519-derived device id).
/// * `boot` is a per-run random id — a peer that sees a new one knows the
///   phone restarted (the same event-driven gossip the desktop uses).
/// * `version` is the app's `CARGO_PKG_VERSION`.
pub fn mobile_profile(
    node: &NodeId,
    cfg: &MobileNodeConfig,
    boot: u64,
    version: impl Into<String>,
) -> NodeProfile {
    let capabilities = mobile_capabilities(node, cfg.scope);
    // The "N things" headline counts what the phone can *offer* (its sources),
    // so a viewer phone reads as a lean node, a host phone as a few more.
    let device_count = capabilities.iter().filter(|c| c.flow.can_source()).count() as u32;
    let label = if cfg.label.trim().is_empty() {
        cfg.model.clone()
    } else {
        cfg.label.clone()
    };

    NodeProfile {
        protocol: PROTOCOL_VERSION,
        node: node.clone(),
        label,
        // A phone has no separate hostname; the label is the whole story.
        hostname: String::new(),
        summary: InventorySummary {
            os: cfg.os.clone(),
            cpu: cfg.model.clone(),
            ram_bytes: cfg.ram_bytes,
            device_count,
        },
        capabilities,
        // Ownership is established by pairing into a fleet, not asserted here.
        owner: None,
        // A phone is never put up for adoption — it claims, it isn't claimed.
        claimable: false,
        boot,
        features: mobile_features(cfg.scope),
        // A phone exposes no reverse-proxied sites.
        sites: Vec::new(),
        version: version.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_carries_the_phones_caps_and_features() {
        let me = NodeId::from("phone-xyz");
        let cfg = MobileNodeConfig {
            label: "Chris's iPhone".into(),
            os: "iOS 18.4".into(),
            model: "iPhone 15 Pro".into(),
            ram_bytes: 8 << 30,
            scope: MobileScope::ViewerController,
        };
        let p = mobile_profile(&me, &cfg, 42, "0.2.4");

        assert_eq!(p.protocol, PROTOCOL_VERSION);
        assert_eq!(p.node.as_str(), "phone-xyz");
        assert_eq!(p.label, "Chris's iPhone");
        assert_eq!(p.summary.os, "iOS 18.4");
        assert_eq!(p.summary.cpu, "iPhone 15 Pro");
        assert!(!p.claimable);
        assert!(p.owner.is_none());
        assert_eq!(p.version, "0.2.4");
        assert!(p.features.iter().any(|f| f == "terminal"));
        assert!(p.capabilities.iter().any(|c| c.origin == "viewer"));
        // A viewer phone offers two source-capable endpoints: touch control
        // (input source) and the clipboard (duplex).
        assert_eq!(p.summary.device_count, 2);

        // The whole profile round-trips through JSON like any other peer's.
        let json = serde_json::to_string(&p).unwrap();
        let back: NodeProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn empty_label_falls_back_to_model() {
        let me = NodeId::from("phone-1");
        let cfg = MobileNodeConfig {
            label: "  ".into(),
            os: "Android 15".into(),
            model: "Pixel 9".into(),
            ram_bytes: 0,
            scope: MobileScope::ViewerControllerHost,
        };
        let p = mobile_profile(&me, &cfg, 1, "0.2.4");
        assert_eq!(p.label, "Pixel 9");
        // Host scope adds camera + mic + screen sources on top of the viewer's
        // two (touch control + clipboard) → 5 offerable endpoints.
        assert_eq!(p.summary.device_count, 5);
    }
}
