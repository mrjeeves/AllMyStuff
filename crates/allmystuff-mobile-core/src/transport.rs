//! The seam between this pure logic and the embedded mesh.
//!
//! On device, the app links `myownmesh-core` (cross-compiled, via a UniFFI
//! layer) and implements [`MeshClient`] over a [`JoinedNetwork`]: presence,
//! the control channel, and the media channel all map onto the engine's typed
//! `Channel<T>` API. In tests, an in-memory fake implements the same trait, so
//! every offer/observe path in this crate runs without a radio.
//!
//! Inbound traffic arrives as raw `(channel, from, payload)` triples off the
//! mesh; [`classify`] turns one into a typed [`Inbound`] using the channel
//! constants and the same lenient decoders the desktop uses — an
//! unrecognised payload yields `None` (or a `*::Unknown`), never an error, so
//! a newer peer can't knock the phone off the channel.
//!
//! [`JoinedNetwork`]: https://docs.rs/myownmesh-core

use allmystuff_protocol::{
    ControlMessage, NodeProfile, OwnedRoster, RoomMessage, CHANNEL_CONTROL, CHANNEL_MEDIA,
    CHANNEL_OWNED, CHANNEL_PRESENCE, CHANNEL_ROOMS,
};
use allmystuff_session::MediaPayload;

/// What couldn't be done over the mesh.
#[derive(Debug, thiserror::Error)]
pub enum MeshError {
    /// No live network to send on (not yet joined, or torn down).
    #[error("not connected to a network")]
    NotConnected,
    /// The peer is unknown / unreachable on this network.
    #[error("no such peer: {0}")]
    NoSuchPeer(String),
    /// The engine refused or failed the send; carries its message.
    #[error("mesh send failed: {0}")]
    Send(String),
    /// A payload wouldn't serialize.
    #[error(transparent)]
    Encode(#[from] serde_json::Error),
}

pub type MeshResult<T> = Result<T, MeshError>;

/// One typed thing the phone took off the mesh.
#[derive(Debug, Clone, PartialEq)]
pub enum Inbound {
    /// A peer's presence advert (its [`NodeProfile`] carries its node id).
    Presence(NodeProfile),
    /// A control message from `from` — route offers/accepts, share/ownership
    /// negotiation, app control.
    Control { from: String, msg: ControlMessage },
    /// A media-channel frame from `from` — video/audio/input/terminal/files/
    /// clipboard/site. Feed it to the matching plane in [`crate::media`].
    Media { from: String, payload: MediaPayload },
    /// A fleet roster update (the owned-devices channel).
    Owned(OwnedRoster),
    /// A rooms-channel message from `from`.
    Room { from: String, msg: RoomMessage },
}

/// Turn one raw inbound `(channel, from, payload)` into a typed [`Inbound`].
///
/// `None` when the channel isn't one AllMyStuff speaks, or the payload doesn't
/// decode — both are dropped silently, the forward-compatible default. A
/// control/room message with a tag this build doesn't know decodes to its
/// enum's `Unknown` variant rather than `None`, so the envelope still arrives
/// (and is ignored downstream).
pub fn classify(channel: &str, from: &str, payload: serde_json::Value) -> Option<Inbound> {
    match channel {
        CHANNEL_PRESENCE => serde_json::from_value(payload).ok().map(Inbound::Presence),
        CHANNEL_CONTROL => serde_json::from_value(payload)
            .ok()
            .map(|msg| Inbound::Control {
                from: from.to_string(),
                msg,
            }),
        CHANNEL_MEDIA => MediaPayload::decode(payload).map(|payload| Inbound::Media {
            from: from.to_string(),
            payload,
        }),
        CHANNEL_OWNED => serde_json::from_value(payload).ok().map(Inbound::Owned),
        CHANNEL_ROOMS => serde_json::from_value(payload)
            .ok()
            .map(|msg| Inbound::Room {
                from: from.to_string(),
                msg,
            }),
        _ => None,
    }
}

/// The outbound mesh surface a phone needs — the small slice of
/// `myownmesh-core`'s `JoinedNetwork` this crate drives. The platform layer
/// implements it over the embedded engine; tests implement an in-memory fake.
///
/// All sends name a `peer` (its mesh device id), because AllMyStuff publishes
/// to specific peers, not the whole room — a route offer goes to the one
/// machine it's for.
pub trait MeshClient: Send + Sync {
    /// This phone's mesh device id (its ed25519-derived public id).
    fn device_id(&self) -> String;

    /// Publish/refresh this phone's presence (its [`NodeProfile`]) to the
    /// network. Called on join and whenever the profile changes.
    fn advertise(&self, profile: &NodeProfile) -> MeshResult<()>;

    /// The device ids of peers the engine currently sees on this network.
    fn peers(&self) -> Vec<String>;

    /// Send a control message to one peer on [`CHANNEL_CONTROL`].
    fn send_control(&self, peer: &str, msg: &ControlMessage) -> MeshResult<()>;

    /// Send a pre-serialized media frame to one peer on [`CHANNEL_MEDIA`].
    /// Take a [`serde_json::Value`] so any of the plane frame types
    /// (`InputEvent`, `TermFrame`, `FileFrame`, …) can be sent uniformly.
    fn send_media(&self, peer: &str, payload: &serde_json::Value) -> MeshResult<()>;

    /// Serialize a typed media frame and send it. Provided over
    /// [`MeshClient::send_media`] so callers hand it a `TermFrame` /
    /// `InputEvent` / `FileFrame` directly.
    fn send_frame<T: serde::Serialize>(&self, peer: &str, frame: &T) -> MeshResult<()> {
        let value = serde_json::to_value(frame)?;
        self.send_media(peer, &value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connect::{offer_terminal, route_id};
    use crate::media::TermPlane;
    use allmystuff_graph::NodeId;
    use allmystuff_protocol::RouteControl;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeMesh {
        id: String,
        sent_control: Mutex<Vec<(String, ControlMessage)>>,
        sent_media: Mutex<Vec<(String, serde_json::Value)>>,
        advertised: Mutex<Vec<NodeProfile>>,
        peers: Vec<String>,
    }

    impl MeshClient for FakeMesh {
        fn device_id(&self) -> String {
            self.id.clone()
        }
        fn advertise(&self, profile: &NodeProfile) -> MeshResult<()> {
            self.advertised.lock().unwrap().push(profile.clone());
            Ok(())
        }
        fn peers(&self) -> Vec<String> {
            self.peers.clone()
        }
        fn send_control(&self, peer: &str, msg: &ControlMessage) -> MeshResult<()> {
            self.sent_control
                .lock()
                .unwrap()
                .push((peer.to_string(), msg.clone()));
            Ok(())
        }
        fn send_media(&self, peer: &str, payload: &serde_json::Value) -> MeshResult<()> {
            self.sent_media
                .lock()
                .unwrap()
                .push((peer.to_string(), payload.clone()));
            Ok(())
        }
    }

    #[test]
    fn classify_routes_each_channel_to_its_type() {
        // A control Accept on the control channel.
        let accept = serde_json::json!({
            "t": "route", "kind": "accept", "route_id": "route:x→y", "session": "term-2"
        });
        match classify(CHANNEL_CONTROL, "desk", accept) {
            Some(Inbound::Control {
                from,
                msg: ControlMessage::Route(RouteControl::Accept { route_id, session }),
            }) => {
                assert_eq!(from, "desk");
                assert_eq!(route_id, "route:x→y");
                assert_eq!(session.as_deref(), Some("term-2"));
            }
            other => panic!("expected a route accept, got {other:?}"),
        }

        // A terminal data frame on the media channel.
        let term = serde_json::json!({
            "t": "term", "route": "route:r", "seq": 0, "kind": "data", "bytes": "aGk="
        });
        assert!(matches!(
            classify(CHANNEL_MEDIA, "desk", term),
            Some(Inbound::Media {
                payload: MediaPayload::Terminal(_),
                ..
            })
        ));

        // An unknown channel is dropped.
        assert!(classify("some/other/channel", "desk", serde_json::json!({})).is_none());
    }

    #[test]
    fn end_to_end_open_a_terminal_against_the_fake_mesh() {
        let phone = NodeId::from("phone");
        let desk = NodeId::from("desk");
        let mesh = FakeMesh {
            id: "phone".into(),
            peers: vec!["desk".into()],
            ..Default::default()
        };

        // Offer a terminal to the desk.
        let offer = offer_terminal(&desk, &phone, None, "tab-1");
        mesh.send_control("desk", &offer).unwrap();
        let rid = route_id("desk:terminal", "phone:term-view:tab-1");

        // The desk accepts (its reply arrives off the control channel).
        let accept = classify(
            CHANNEL_CONTROL,
            "desk",
            serde_json::json!({ "t": "route", "kind": "accept", "route_id": rid, "session": "term-7" }),
        );
        assert!(matches!(
            accept,
            Some(Inbound::Control {
                msg: ControlMessage::Route(RouteControl::Accept { .. }),
                ..
            })
        ));

        // Type a command; it goes out on the media channel as a term frame.
        let mut term = TermPlane::new(&rid);
        mesh.send_frame("desk", &term.send(b"ls\n".to_vec()))
            .unwrap();

        let media = mesh.sent_media.lock().unwrap();
        assert_eq!(media.len(), 1);
        assert_eq!(media[0].0, "desk");
        assert_eq!(media[0].1["t"], "term");
        assert_eq!(media[0].1["kind"], "data");

        let ctl = mesh.sent_control.lock().unwrap();
        assert_eq!(ctl.len(), 1);
        assert!(matches!(
            ctl[0].1,
            ControlMessage::Route(RouteControl::Offer { .. })
        ));
    }
}
