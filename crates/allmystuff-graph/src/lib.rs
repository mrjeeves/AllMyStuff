//! # allmystuff-graph
//!
//! The model behind the AllMyStuff screen: a graph of **nodes** (your
//! machines and the people you share with), the **capabilities** each one
//! exposes (this mic, that display, this computer's screen), and the
//! **routes** that wire one capability to another across the mesh.
//!
//! On top of that sits the part that makes it safe for normal people: the
//! **relationship + grant** model. MyOwnMesh underneath proves *identity*
//! (who a peer cryptographically is). This crate never surfaces keys — it
//! answers the only question a user actually has: *is this mine, or am I
//! sharing with someone, and for what?* Every route is checked against
//! that. See [`Relationship`] and [`Catalog::authorize`].
//!
//! ```
//! use allmystuff_graph::*;
//!
//! let mut cat = Catalog::new();
//! cat.nodes.push(MeshNode::this("My laptop"));
//! cat.nodes.push(MeshNode {
//!     id: "desk-pc".into(),
//!     label: "Desk PC".into(),
//!     kind: NodeKind::Machine,
//!     relationship: Relationship::Mine, // a device I own
//!     online: true,
//! });
//! cat.capabilities.push(Capability::new(
//!     NodeId::this(), "this:mic", "Laptop mic", MediaKind::Audio, Flow::Source, "microphone",
//! ));
//! cat.capabilities.push(Capability::new(
//!     "desk-pc", "desk-pc:system-in", "Desk PC audio in", MediaKind::Audio, Flow::Sink, "system",
//! ));
//!
//! // Both are mine, so this just works — no grant ceremony.
//! let route = cat.propose_route(&"this:mic".into(), &"desk-pc:system-in".into()).unwrap();
//! assert_eq!(route.media, MediaKind::Audio);
//! ```

mod authz;
mod catalog;
mod model;

pub use authz::{describe_action, describe_grant, Denied, GrantRequest};
pub use catalog::{Catalog, ConnectError};
pub use model::*;

#[cfg(test)]
mod tests {
    use super::*;

    // ---- fixtures -----------------------------------------------------

    fn alex() -> Person {
        Person {
            id: "person:alex".into(),
            name: "Alex".into(),
        }
    }

    /// A catalog with: this laptop (mine), a desk PC I own, and Alex's
    /// laptop (shared, no grants yet). Each machine carries the synthetic
    /// "screen" (Display source) + "control" (Input sink) + "system" audio
    /// duplex capabilities the bridge would mint, plus a physical display
    /// and mic on *this* node.
    fn fixture() -> Catalog {
        let mut cat = Catalog::new();
        cat.nodes.push(MeshNode::this("My laptop"));
        cat.nodes.push(MeshNode {
            id: "desk".into(),
            label: "Desk PC".into(),
            kind: NodeKind::Machine,
            relationship: Relationship::Mine,
            online: true,
        });
        cat.nodes.push(MeshNode {
            id: "alex".into(),
            label: "Alex's laptop".into(),
            kind: NodeKind::Machine,
            relationship: Relationship::Shared(Share {
                person: alex(),
                grants: vec![],
            }),
            online: true,
        });

        for node in ["this", "desk", "alex"] {
            cat.capabilities.push(Capability::new(
                node,
                format!("{node}:screen"),
                "Screen",
                MediaKind::Display,
                Flow::Source,
                "screen",
            ));
            cat.capabilities.push(Capability::new(
                node,
                format!("{node}:control"),
                "Keyboard & mouse control",
                MediaKind::Input,
                Flow::Sink,
                "control",
            ));
            cat.capabilities.push(Capability::new(
                node,
                format!("{node}:system"),
                "System audio",
                MediaKind::Audio,
                Flow::Duplex,
                "system",
            ));
        }

        // Physical peripherals on this laptop.
        cat.capabilities.push(Capability::new(
            "this",
            "this:display",
            "Built-in display",
            MediaKind::Display,
            Flow::Sink,
            "display",
        ));
        cat.capabilities.push(Capability::new(
            "this",
            "this:mic",
            "Mic array",
            MediaKind::Audio,
            Flow::Source,
            "microphone",
        ));
        cat.capabilities.push(Capability::new(
            "this",
            "this:keyboard",
            "Keyboard",
            MediaKind::Input,
            Flow::Source,
            "keyboard",
        ));
        cat.capabilities.push(Capability::new(
            "this",
            "this:speaker",
            "Speakers",
            MediaKind::Audio,
            Flow::Sink,
            "speaker",
        ));
        cat
    }

    // ---- routing basics ----------------------------------------------

    #[test]
    fn mine_to_mine_connects_without_grants() {
        let cat = fixture();
        let r = cat
            .propose_route(&"this:mic".into(), &"desk:system".into())
            .expect("two of my own devices connect freely");
        assert_eq!(r.media, MediaKind::Audio);
        assert_eq!(r.from, "this:mic".into());
    }

    #[test]
    fn media_mismatch_is_rejected() {
        let cat = fixture();
        // A mic (audio) can't feed a display sink.
        let err = cat
            .propose_route(&"this:mic".into(), &"this:display".into())
            .unwrap_err();
        assert!(matches!(err, ConnectError::MediaMismatch { .. }), "{err:?}");
    }

    #[test]
    fn wrong_flow_is_rejected() {
        let cat = fixture();
        // A display *sink* can't be a source.
        let err = cat
            .propose_route(&"this:display".into(), &"desk:screen".into())
            .unwrap_err();
        assert!(matches!(err, ConnectError::WrongFlow { .. }), "{err:?}");
    }

    #[test]
    fn self_loop_is_rejected() {
        let cat = fixture();
        let err = cat
            .propose_route(&"this:mic".into(), &"this:mic".into())
            .unwrap_err();
        assert_eq!(err, ConnectError::SelfLoop);
    }

    // ---- authorization -----------------------------------------------

    #[test]
    fn sharing_to_a_person_is_denied_without_a_grant() {
        let mut cat = fixture();
        cat.capabilities.push(Capability::new(
            "alex",
            "alex:display",
            "Alex's monitor",
            MediaKind::Display,
            Flow::Sink,
            "display",
        ));
        // Cast my screen to Alex's display — Alex's endpoint is a sink
        // (Consume), and there's no grant.
        let err = cat
            .propose_route(&"this:screen".into(), &"alex:display".into())
            .unwrap_err();
        match err {
            ConnectError::Denied(d) => {
                assert_eq!(d.person_name, "Alex");
                assert_eq!(d.role, GrantRole::Consume);
                assert_eq!(d.media, MediaKind::Display);
            }
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    #[test]
    fn a_matching_grant_authorizes_the_share() {
        let mut cat = fixture();
        // Need a display sink on Alex's side.
        cat.capabilities.push(Capability::new(
            "alex",
            "alex:display",
            "Alex's monitor",
            MediaKind::Display,
            Flow::Sink,
            "display",
        ));
        // Grant Alex "may receive my display."
        grant(
            &mut cat,
            "alex",
            Grant {
                id: "g1".into(),
                media: MediaKind::Display,
                role: GrantRole::Consume,
                capability: None,
                label: "Receive your screen".into(),
            },
        );
        let r = cat
            .propose_route(&"this:screen".into(), &"alex:display".into())
            .expect("grant authorizes the cast");
        assert_eq!(r.media, MediaKind::Display);
    }

    #[test]
    fn grant_direction_matters() {
        let mut cat = fixture();
        cat.capabilities.push(Capability::new(
            "alex",
            "alex:cam",
            "Alex's webcam",
            MediaKind::Video,
            Flow::Source,
            "camera",
        ));
        cat.capabilities.push(Capability::new(
            "this",
            "this:videowin",
            "Video window",
            MediaKind::Video,
            Flow::Sink,
            "screen",
        ));
        // A "Consume" grant does NOT let Alex be a source.
        grant(
            &mut cat,
            "alex",
            Grant {
                id: "g-consume".into(),
                media: MediaKind::Video,
                role: GrantRole::Consume,
                capability: None,
                label: String::new(),
            },
        );
        let err = cat
            .propose_route(&"alex:cam".into(), &"this:videowin".into())
            .unwrap_err();
        assert!(matches!(err, ConnectError::Denied(_)));

        // A "Provide" grant does.
        grant(
            &mut cat,
            "alex",
            Grant {
                id: "g-provide".into(),
                media: MediaKind::Video,
                role: GrantRole::Provide,
                capability: None,
                label: String::new(),
            },
        );
        cat.propose_route(&"alex:cam".into(), &"this:videowin".into())
            .expect("provide grant lets Alex send video");
    }

    #[test]
    fn grant_can_be_pinned_to_one_capability() {
        let mut cat = fixture();
        cat.capabilities.push(Capability::new(
            "alex",
            "alex:spkA",
            "Kitchen speaker",
            MediaKind::Audio,
            Flow::Sink,
            "speaker",
        ));
        cat.capabilities.push(Capability::new(
            "alex",
            "alex:spkB",
            "Bedroom speaker",
            MediaKind::Audio,
            Flow::Sink,
            "speaker",
        ));
        // Grant pinned to the kitchen speaker only.
        grant(
            &mut cat,
            "alex",
            Grant {
                id: "g-kitchen".into(),
                media: MediaKind::Audio,
                role: GrantRole::Consume,
                capability: Some("alex:spkA".into()),
                label: String::new(),
            },
        );
        cat.propose_route(&"this:mic".into(), &"alex:spkA".into())
            .expect("kitchen is granted");
        let err = cat
            .propose_route(&"this:mic".into(), &"alex:spkB".into())
            .unwrap_err();
        assert!(
            matches!(err, ConnectError::Denied(_)),
            "bedroom is not granted"
        );
    }

    #[test]
    fn a_grant_covers_every_node_the_same_person_brings() {
        // Sharing authorizes the *person*: a grant recorded on one of
        // Alex's machines lets Alex route the granted thing to any of
        // their nodes — here, a second laptop that arrived later.
        let mut cat = fixture();
        cat.nodes.push(MeshNode {
            id: "alex2".into(),
            label: "Alex's studio PC".into(),
            kind: NodeKind::Machine,
            relationship: Relationship::Shared(Share {
                person: alex(), // same person, second machine
                grants: vec![],
            }),
            online: true,
        });
        cat.capabilities.push(Capability::new(
            "alex2",
            "alex2:display",
            "Studio monitor",
            MediaKind::Display,
            Flow::Sink,
            "display",
        ));
        // The grant lives on the *first* node's share…
        grant(
            &mut cat,
            "alex",
            Grant {
                id: "g1".into(),
                media: MediaKind::Display,
                role: GrantRole::Consume,
                capability: None,
                label: "Receive your screen".into(),
            },
        );
        // …and authorizes the same person's *other* machine too.
        cat.propose_route(&"this:screen".into(), &"alex2:display".into())
            .expect("the person is granted, whichever of their nodes receives");

        // A different person with their own node is still denied.
        cat.nodes.push(MeshNode {
            id: "sam".into(),
            label: "Sam's laptop".into(),
            kind: NodeKind::Machine,
            relationship: Relationship::Shared(Share {
                person: Person {
                    id: "person:sam".into(),
                    name: "Sam".into(),
                },
                grants: vec![],
            }),
            online: true,
        });
        cat.capabilities.push(Capability::new(
            "sam",
            "sam:display",
            "Sam's monitor",
            MediaKind::Display,
            Flow::Sink,
            "display",
        ));
        let err = cat
            .propose_route(&"this:screen".into(), &"sam:display".into())
            .unwrap_err();
        assert!(matches!(err, ConnectError::Denied(_)), "{err:?}");
    }

    #[test]
    fn required_grants_describes_the_one_tap_fix() {
        let mut cat = fixture();
        cat.capabilities.push(Capability::new(
            "alex",
            "alex:display",
            "Alex's monitor",
            MediaKind::Display,
            Flow::Sink,
            "display",
        ));
        let reqs = cat.required_grants(&"this:screen".into(), &"alex:display".into());
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].person_name, "Alex");
        assert_eq!(reqs[0].role, GrantRole::Consume);
        assert_eq!(reqs[0].capability, Some("alex:display".into()));
        assert_eq!(reqs[0].description, "Receive your display");
    }

    #[test]
    fn room_routes_skip_the_grant_gate_but_not_the_rules() {
        // Casting my screen to Alex's display has no grant — the normal
        // path denies it, while the rooms plane (membership is the
        // consent, scoped to the room) lets it through.
        let mut cat = fixture();
        cat.capabilities.push(Capability::new(
            "alex",
            "alex:display",
            "Alex's monitor",
            MediaKind::Display,
            Flow::Sink,
            "display",
        ));
        let from: CapabilityId = "this:screen".into();
        let to: CapabilityId = "alex:display".into();
        assert!(matches!(
            cat.propose_route(&from, &to).unwrap_err(),
            ConnectError::Denied(_)
        ));
        let r = cat
            .propose_room_route(&from, &to)
            .expect("a room leg needs no grant");
        assert_eq!(r.media, MediaKind::Display);

        // The structural rules still hold: a mic can't feed a display.
        let err = cat
            .propose_room_route(&"this:mic".into(), &"alex:display".into())
            .unwrap_err();
        assert!(matches!(err, ConnectError::MediaMismatch { .. }), "{err:?}");
    }

    // ---- endpoint matching --------------------------------------------

    #[test]
    fn match_endpoint_prefers_machine_then_default_device() {
        // On a node with a synthetic system endpoint, audio lands there.
        let cat = fixture();
        let picked = cat
            .match_endpoint(&"desk".into(), MediaKind::Audio, GrantRole::Consume)
            .expect("desk can sink audio");
        assert_eq!(picked.id, "desk:system".into());

        // Without one, the category's *default* device wins over id order.
        let mut cat = Catalog::new();
        cat.nodes.push(MeshNode::this("My laptop"));
        cat.nodes.push(MeshNode {
            id: "box".into(),
            label: "Box".into(),
            kind: NodeKind::Machine,
            relationship: Relationship::Mine,
            online: true,
        });
        // The default is the id-*later* speaker, so only default-preference
        // (not id order, which would pick spkA) can explain the choice.
        cat.capabilities.push(Capability::new(
            "box",
            "box:spkA",
            "Aux speaker",
            MediaKind::Audio,
            Flow::Sink,
            "speaker",
        ));
        cat.capabilities.push(
            Capability::new(
                "box",
                "box:spkZ",
                "Main speaker",
                MediaKind::Audio,
                Flow::Sink,
                "speaker",
            )
            .as_default(true),
        );
        let picked = cat
            .match_endpoint(&"box".into(), MediaKind::Audio, GrantRole::Consume)
            .expect("box can sink audio");
        assert_eq!(picked.id, "box:spkZ".into());

        // Storage auto-routing must land on the app's dedicated mapped-drive
        // sink, not loop back into an attached physical disk (even a default).
        cat.capabilities.push(
            Capability::new(
                "box",
                "box:disk:C:\\",
                "System drive",
                MediaKind::Storage,
                Flow::Duplex,
                "disk",
            )
            .as_default(true),
        );
        cat.capabilities.push(Capability::new(
            "box",
            "box:storage-in",
            "Mapped drives",
            MediaKind::Storage,
            Flow::Sink,
            "storage-in",
        ));
        let picked = cat
            .match_endpoint(&"box".into(), MediaKind::Storage, GrantRole::Consume)
            .expect("box can receive a mapped drive");
        assert_eq!(picked.id, "box:storage-in".into());
    }

    // ---- serde --------------------------------------------------------

    #[test]
    fn catalog_round_trips_through_json() {
        let cat = fixture();
        let json = serde_json::to_string(&cat).unwrap();
        let back: Catalog = serde_json::from_str(&json).unwrap();
        assert_eq!(cat, back);

        // Relationship is internally tagged so TS can switch on `kind`.
        assert!(json.contains(r#""kind":"mine""#));
        assert!(json.contains(r#""kind":"shared""#));
        // Ids serialise as bare strings, not wrapper objects.
        assert!(json.contains(r#""id":"this""#));
    }

    // ---- grant ids (stable, content-derived) --------------------------

    #[test]
    fn scoped_grant_id_is_stable_for_the_same_scope() {
        let alex = alex().id;
        let a = Grant::scoped(
            &alex,
            MediaKind::Display,
            GrantRole::Consume,
            None,
            "Receive your screen",
        );
        // Same scope, different label → same id (it's derived from the scope,
        // not the wording), so it de-dupes and a restart reloads to it.
        let b = Grant::scoped(
            &alex,
            MediaKind::Display,
            GrantRole::Consume,
            None,
            "Cast to them",
        );
        assert_eq!(a.id, b.id);
        assert_eq!(a.id, "grant:person:alex:display:consume:*");
        assert_eq!(
            a.id,
            Grant::id_for(&alex, MediaKind::Display, GrantRole::Consume, None)
        );
    }

    #[test]
    fn scoped_grant_id_separates_role_media_capability_and_person() {
        let alex = alex().id;
        let bo: PersonId = "person:bo".into();
        let cap: CapabilityId = "alex:speaker".into();
        let base = Grant::scoped(&alex, MediaKind::Audio, GrantRole::Consume, None, "").id;
        // Each axis that authorization keys on changes the id.
        assert_ne!(
            base,
            Grant::scoped(&alex, MediaKind::Audio, GrantRole::Provide, None, "").id
        );
        assert_ne!(
            base,
            Grant::scoped(&alex, MediaKind::Video, GrantRole::Consume, None, "").id
        );
        assert_ne!(
            base,
            Grant::scoped(&alex, MediaKind::Audio, GrantRole::Consume, Some(cap), "").id
        );
        assert_ne!(
            base,
            Grant::scoped(&bo, MediaKind::Audio, GrantRole::Consume, None, "").id
        );
    }

    #[test]
    fn scoped_grant_still_permits_the_route_it_describes() {
        // The deterministic id doesn't change what the grant authorizes.
        let alex = alex().id;
        let cap: CapabilityId = "alex:screen".into();
        let g = Grant::scoped(
            &alex,
            MediaKind::Display,
            GrantRole::Consume,
            Some(cap.clone()),
            "Receive your screen",
        );
        assert!(g.permits(MediaKind::Display, GrantRole::Consume, &cap));
        // Wrong direction or a different pinned capability is not covered.
        assert!(!g.permits(MediaKind::Display, GrantRole::Provide, &cap));
        assert!(!g.permits(MediaKind::Display, GrantRole::Consume, &"alex:other".into()));
    }

    // ---- helpers ------------------------------------------------------

    fn grant(cat: &mut Catalog, node: &str, g: Grant) {
        let node = cat
            .nodes
            .iter_mut()
            .find(|n| n.id == node.into())
            .expect("node exists");
        if let Relationship::Shared(share) = &mut node.relationship {
            share.grants.push(g);
        } else {
            panic!("node is not shared");
        }
    }
}
