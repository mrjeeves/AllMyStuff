//! Golden wire-contract fixtures for the NanoKVM ↔ AllMyStuff bridge.
//!
//! The NanoKVM mesh bridge is written in Go and hand-mirrors these JSON
//! shapes; a drift between the Go structs and the Rust source here would make
//! peers **silently** drop the KVM (a JSON parse error on the receiving end,
//! never a visible failure). To catch that at CI time instead of in the field,
//! this example serialises canonical instances of every wire type the bridge
//! touches and writes them as `<name>.json`. The Go side commits a copy and
//! round-trips each fixture in its own tests, pinned to this protocol version.
//!
//! Regenerate after any protocol change:
//!
//! ```sh
//! cargo run -p allmystuff-session --example dump_kvm_fixtures -- contract-fixtures
//! ```
//!
//! The single source of truth is the Rust types in `allmystuff-protocol` and
//! `allmystuff-session` — never edit the generated JSON by hand.

use std::collections::BTreeMap;
use std::path::PathBuf;

use allmystuff_graph::{Capability, Flow, MediaKind, Route};
use allmystuff_protocol::app::{
    InventorySummary, KvmAdvert, NodeProfile, OwnershipControl, RouteControl, SiteAdvert,
};
use allmystuff_protocol::control::{ClientId, Request, Response, ServerOut};
use allmystuff_protocol::{ControlMessage, KvmControl, PROTOCOL_VERSION};
use allmystuff_session::{SiteEvent, SiteFrame};
use serde_json::{json, Value};

fn main() {
    let out = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("contract-fixtures"));
    std::fs::create_dir_all(&out).expect("create fixtures dir");

    let mut fixtures: BTreeMap<&str, Value> = BTreeMap::new();

    // -- presence: a KVM appliance node ---------------------------------
    // What a NanoKVM broadcasts on CHANNEL_PRESENCE once joined + claimed:
    // tagged `kvm` + `sites`, advertising its web UI as a site, and bound
    // (attached) to the machine it controls.
    let kvm_profile = NodeProfile {
        protocol: PROTOCOL_VERSION,
        node: "kvm-abcdef".into(),
        label: "NanoKVM (den-tower)".into(),
        hostname: "nanokvm".into(),
        summary: InventorySummary {
            os: "linux".into(),
            cpu: "SG2002 (T-Head C906)".into(),
            ram_bytes: 256 << 20,
            device_count: 2,
            product: "NanoKVM".into(),
        },
        capabilities: vec![
            Capability::new(
                "kvm-abcdef",
                "kvm-abcdef:screen",
                "Captured screen",
                MediaKind::Display,
                Flow::Source,
                "screen",
            ),
            Capability::new(
                "kvm-abcdef",
                "kvm-abcdef:control",
                "Keyboard & mouse",
                MediaKind::Input,
                Flow::Sink,
                "control",
            ),
        ],
        owner: Some("den-tower".into()),
        claimable: false,
        boot: 42,
        features: vec!["kvm".into(), "sites".into()],
        sites: vec![SiteAdvert {
            id: "tcp:80".into(),
            label: "KVM Web UI".into(),
            port: 80,
            scheme: "http".into(),
            loopback: false,
        }],
        version: "0.2.6".into(),
        fleet_name: "Casey".into(),
        fleet_owner: "Casey".into(),
        kvm: Some(KvmAdvert {
            attached_to: Some("den-tower".into()),
            web: "tcp:80".into(),
            // The per-device joining mesh (derived on the KVM from its own
            // identity; this value is just a well-formed example) plus the
            // full membership list — fleet mesh included.
            joining_mesh: "cec-kvm-ab3de-fg7hj".into(),
            meshes: vec!["amber-turing-x3k9q".into(), "den-site-mesh".into()],
            virtual_media: None,
        }),
        // 0 = unstamped: keeps the pinned Go fixtures byte-identical (zero
        // serializes without the key). Real sends stamp it per send.
        sent_at: 0,
    };
    fixtures.insert("node_profile_kvm", to_value(&kvm_profile));

    // A freshly-booted, unclaimed KVM: claimable, no owner, not yet attached,
    // sitting alone on its own joining mesh.
    let mut claimable = kvm_profile.clone();
    claimable.owner = None;
    claimable.claimable = true;
    claimable.fleet_name = String::new();
    claimable.fleet_owner = String::new();
    claimable.kvm = Some(KvmAdvert {
        attached_to: None,
        web: "tcp:80".into(),
        joining_mesh: "cec-kvm-ab3de-fg7hj".into(),
        meshes: vec!["cec-kvm-ab3de-fg7hj".into()],
        virtual_media: None,
    });
    fixtures.insert("node_profile_kvm_claimable", to_value(&claimable));

    // -- the KVM control plane (CHANNEL_CONTROL) -------------------------
    fixtures.insert(
        "control_kvm_attach",
        to_value(&ControlMessage::Kvm(KvmControl::Attach {
            node: "den-tower".into(),
            label: "Den Tower".into(),
        })),
    );
    fixtures.insert(
        "control_kvm_detach",
        to_value(&ControlMessage::Kvm(KvmControl::Detach)),
    );
    fixtures.insert(
        "control_kvm_mesh_add",
        to_value(&ControlMessage::Kvm(KvmControl::MeshAdd {
            network_id: "den-site-mesh".into(),
        })),
    );
    fixtures.insert(
        "control_kvm_mesh_remove",
        to_value(&ControlMessage::Kvm(KvmControl::MeshRemove {
            network_id: "den-site-mesh".into(),
        })),
    );

    // -- ownership / claim → fleet (CHANNEL_CONTROL) --------------------
    fixtures.insert(
        "control_ownership_claim",
        to_value(&ControlMessage::Ownership(OwnershipControl::Claim {
            owner: "den-tower".into(),
        })),
    );
    fixtures.insert(
        "control_ownership_claimed",
        to_value(&ControlMessage::Ownership(OwnershipControl::Claimed {
            owner: "den-tower".into(),
        })),
    );
    fixtures.insert(
        "control_ownership_fleetkey",
        to_value(&ControlMessage::Ownership(OwnershipControl::FleetKey {
            key: "fleet-secret-key".into(),
            name: "Casey".into(),
            venue: Some("{\"signaling\":{}}".into()),
        })),
    );

    // -- the site route handshake that tunnels the web UI ---------------
    let site_route = Route {
        id: "route:kvm-abcdef:site→den-tower:site-view:80-1".into(),
        from: "kvm-abcdef:site".into(),
        to: "den-tower:site-view:80-1".into(),
        media: MediaKind::Generic,
    };
    fixtures.insert(
        "control_route_offer_site",
        to_value(&ControlMessage::Route(RouteControl::Offer {
            route: site_route.clone(),
            video: Vec::new(),
            audio: Vec::new(),
            session: None,
            drive: None,
            room: None,
        })),
    );
    fixtures.insert(
        "control_route_accept",
        to_value(&ControlMessage::Route(RouteControl::Accept {
            route_id: site_route.id.to_string(),
            session: None,
        })),
    );

    // -- graph capabilities (the endpoints peers route through) ---------
    fixtures.insert(
        "capability_screen",
        to_value(&Capability::new(
            "kvm-abcdef",
            "kvm-abcdef:screen",
            "Captured screen",
            MediaKind::Display,
            Flow::Source,
            "screen",
        )),
    );
    fixtures.insert(
        "capability_control",
        to_value(&Capability::new(
            "kvm-abcdef",
            "kvm-abcdef:control",
            "Keyboard & mouse",
            MediaKind::Input,
            Flow::Sink,
            "control",
        )),
    );

    fixtures.insert(
        "inventory_summary",
        to_value(&InventorySummary {
            os: "linux".into(),
            cpu: "SG2002 (T-Head C906)".into(),
            ram_bytes: 256 << 20,
            device_count: 2,
            product: "NanoKVM".into(),
        }),
    );
    fixtures.insert(
        "site_advert",
        to_value(&SiteAdvert {
            id: "tcp:80".into(),
            label: "KVM Web UI".into(),
            port: 80,
            scheme: "http".into(),
            loopback: false,
        }),
    );

    // -- the site-tunnel media frames (CHANNEL_MEDIA, t="site") ---------
    fixtures.insert(
        "site_frame_open",
        to_value(&SiteFrame::new(
            site_route.id.clone(),
            1,
            SiteEvent::Open { conn: 1, port: 80 },
        )),
    );
    fixtures.insert(
        "site_frame_data",
        to_value(&SiteFrame::new(
            site_route.id.clone(),
            2,
            SiteEvent::Data {
                conn: 1,
                data: b"GET / HTTP/1.1\r\n\r\n".to_vec(),
            },
        )),
    );
    fixtures.insert(
        "site_frame_close",
        to_value(&SiteFrame::new(
            site_route.id.clone(),
            3,
            SiteEvent::Close { conn: 1 },
        )),
    );

    // -- the daemon control socket (line-delimited JSON) ----------------
    // The bridge sends these to ~/.myownmesh/daemon.sock and parses its
    // replies. ClientId rides the wire as the string "c<n>".
    fixtures.insert("client_id", json!(ClientId(7)));
    fixtures.insert(
        "req_channel_subscribe",
        to_value(&Request::ChannelSubscribe {
            client_id: ClientId(7),
            network: "cec-backend-client-mesh".into(),
            channel: "allmystuff/presence/v1".into(),
        }),
    );
    fixtures.insert(
        "req_channel_send_all",
        to_value(&Request::ChannelSendAll {
            network: "cec-backend-client-mesh".into(),
            channel: "allmystuff/presence/v1".into(),
            payload: to_value(&kvm_profile),
        }),
    );
    fixtures.insert(
        "req_capabilities_set",
        to_value(&Request::CapabilitiesSet {
            network: "cec-backend-client-mesh".into(),
            capabilities: json!({
                "tags": ["allmystuff", "kvm", "sites"],
                "app_version": "0.2.6",
                "extra": { "summary": to_value(&kvm_profile.summary) }
            }),
        }),
    );
    fixtures.insert(
        "response_ok",
        to_value(&Response::ok(json!({"client_id": "c7"}))),
    );
    fixtures.insert(
        "server_out_channel_inbound",
        to_value(&ServerOut::ChannelInbound {
            network: "cec-backend-client-mesh".into(),
            from: "den-tower".into(),
            channel: "allmystuff/control/v1".into(),
            // Deliberately label-less: pins the pre-label attach shape an
            // older sender still emits, so the Go side keeps accepting it.
            payload: to_value(&ControlMessage::Kvm(KvmControl::Attach {
                node: "den-tower".into(),
                label: String::new(),
            })),
        }),
    );

    let mut names: Vec<&str> = fixtures.keys().copied().collect();
    names.sort_unstable();
    for name in &names {
        let path = out.join(format!("{name}.json"));
        let mut s = serde_json::to_string_pretty(&fixtures[name]).expect("serialize fixture");
        s.push('\n');
        std::fs::write(&path, s).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    }
    println!("wrote {} fixtures to {}", names.len(), out.display());
}

fn to_value<T: serde::Serialize>(v: &T) -> Value {
    serde_json::to_value(v).expect("serialize to value")
}
