//! Building the [`RouteControl::Offer`] a phone sends to start a session.
//!
//! Two shapes of route exist, and they are negotiated differently:
//!
//! * **Catalog routes** — screen (`display`), camera (`video`), audio. These
//!   wire one advertised [`Capability`] to another and are validated +
//!   authorized through the receiver-side [`Catalog`]
//!   ([`Catalog::propose_route`]). [`offer_route`] and the named helpers
//!   ([`offer_screen`], [`offer_camera`], [`offer_audio`]) take this path.
//!
//! * **Synthetic feature routes** — terminal and files. These don't match
//!   advertised capabilities; the host recognizes them by id pattern
//!   (`<host>:terminal`, `<host>:files`) and authorizes them by *fleet
//!   membership*, not by a grant. [`offer_terminal`] / [`offer_files`] build
//!   them directly, with `media: "generic"` — exactly what `amst` and the
//!   desktop GUI send.
//!
//! Either way the route id is the value both ends independently derive,
//! `route:{from}→{to}` (see [`route_id`]), so the offer the phone sends names
//! the same route the host will key its side on.

use allmystuff_graph::{Capability, CapabilityId, Catalog, GrantRole, MediaKind, NodeId, Route};
use allmystuff_protocol::{ControlMessage, RouteControl};

/// The codecs a phone advertises it can *consume* on a display/camera route,
/// best first. The host picks the best it can produce, falling back to MJPEG
/// over the media channel when the list is empty or nothing matches — the
/// same degradation contract the desktop uses.
pub const VIDEO_CODECS: &[&str] = &["h264"];
/// The audio codecs a phone can consume, best first; empty/none ⇒ PCM frames.
pub const AUDIO_CODECS: &[&str] = &["opus"];

/// What can go wrong assembling an offer.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// The catalog has no endpoint of the needed media+role on that node —
    /// e.g. the remote advertises no screen, or the phone's own sink is
    /// missing from the catalog the caller built.
    #[error("no {role} endpoint for {media} on node {node}")]
    NoEndpoint {
        node: String,
        media: &'static str,
        role: &'static str,
    },
    /// The route failed the catalog's media/flow/authorization checks.
    #[error(transparent)]
    Rejected(#[from] allmystuff_graph::ConnectError),
}

/// The route id both ends derive for a `from`→`to` pair. Matches
/// `allmystuff_graph`'s private `route_id` and the node's own
/// `format!("route:{from}→{to}")` byte-for-byte.
pub fn route_id(from: &str, to: &str) -> String {
    format!("route:{from}→{to}")
}

/// Offer a **catalog-validated** route between two advertised capabilities,
/// carrying the codecs the phone can consume. The route is built and
/// authorized through `catalog` (so an unauthorized or media-mismatched
/// connection is refused here, before anything hits the wire) and wrapped in
/// a [`ControlMessage::Route`] ready to publish on the control channel.
pub fn offer_route(
    catalog: &Catalog,
    from: &CapabilityId,
    to: &CapabilityId,
    video: Vec<String>,
    audio: Vec<String>,
) -> Result<ControlMessage, ConnectError> {
    let route = catalog.propose_route(from, to)?;
    Ok(ControlMessage::Route(RouteControl::Offer {
        route,
        video,
        audio,
        session: None,
    }))
}

/// Watch a remote machine's **desktop** on this phone: the remote's screen
/// (a `Display` source) → the phone's `display-in` (a `Display` sink),
/// offering H.264 with the usual MJPEG fallback.
///
/// When the remote exposes several screens, this picks its default/first via
/// [`Catalog::match_endpoint`]; to target a specific monitor
/// (`<remote>:screen:<id>`), look the capability up yourself and call
/// [`offer_route`].
pub fn offer_screen(
    catalog: &Catalog,
    remote: &NodeId,
    me: &NodeId,
) -> Result<ControlMessage, ConnectError> {
    let from = endpoint(catalog, remote, MediaKind::Display, GrantRole::Provide)?;
    let to = endpoint(catalog, me, MediaKind::Display, GrantRole::Consume)?;
    offer_route(catalog, &from, &to, codecs(VIDEO_CODECS), Vec::new())
}

/// View a remote machine's **camera** on this phone: a `Video` source → the
/// phone's `video-in` (a `Video` sink).
pub fn offer_camera(
    catalog: &Catalog,
    remote: &NodeId,
    me: &NodeId,
) -> Result<ControlMessage, ConnectError> {
    let from = endpoint(catalog, remote, MediaKind::Video, GrantRole::Provide)?;
    let to = endpoint(catalog, me, MediaKind::Video, GrantRole::Consume)?;
    offer_route(catalog, &from, &to, codecs(VIDEO_CODECS), Vec::new())
}

/// Listen to a remote machine's **audio** on this phone: an `Audio` source →
/// the phone's `audio-out` (an `Audio` sink), offering Opus with PCM fallback.
pub fn offer_audio(
    catalog: &Catalog,
    remote: &NodeId,
    me: &NodeId,
) -> Result<ControlMessage, ConnectError> {
    let from = endpoint(catalog, remote, MediaKind::Audio, GrantRole::Provide)?;
    let to = endpoint(catalog, me, MediaKind::Audio, GrantRole::Consume)?;
    offer_route(catalog, &from, &to, Vec::new(), codecs(AUDIO_CODECS))
}

/// Open a **terminal** on `host` from this phone (`me`).
///
/// `attach` joins an already-running shell by its host-side session id
/// (tmux-style multi-attach); `None` mints a fresh shell. `nonce` makes the
/// viewer-side endpoint unique across this phone's open terminals — use a
/// per-tab counter or timestamp (this crate has no clock). No catalog: a
/// terminal route is recognized by id and authorized by fleet membership on
/// the host.
pub fn offer_terminal(
    host: &NodeId,
    me: &NodeId,
    attach: Option<String>,
    nonce: &str,
) -> ControlMessage {
    let from = format!("{}:terminal", host.as_str());
    let to = format!("{}:term-view:{nonce}", me.as_str());
    ControlMessage::Route(RouteControl::Offer {
        route: generic_route(from, to),
        video: Vec::new(),
        audio: Vec::new(),
        session: attach,
    })
}

/// Browse `host`'s **files** from this phone (`me`). Like [`offer_terminal`],
/// a synthetic fleet-gated route; `nonce` keeps concurrent browsers distinct.
pub fn offer_files(host: &NodeId, me: &NodeId, nonce: &str) -> ControlMessage {
    let from = format!("{}:files", host.as_str());
    let to = format!("{}:files-view:{nonce}", me.as_str());
    ControlMessage::Route(RouteControl::Offer {
        route: generic_route(from, to),
        video: Vec::new(),
        audio: Vec::new(),
        session: None,
    })
}

/// Tear a route down from this side — either end may. A thin wrapper so the
/// caller doesn't hand-roll the control message.
pub fn teardown(route_id: impl Into<String>) -> ControlMessage {
    ControlMessage::Route(RouteControl::Teardown {
        route_id: route_id.into(),
    })
}

// ---- helpers ----------------------------------------------------------

fn generic_route(from: String, to: String) -> Route {
    Route {
        id: route_id(&from, &to),
        from: from.into(),
        to: to.into(),
        media: MediaKind::Generic,
    }
}

fn endpoint(
    catalog: &Catalog,
    node: &NodeId,
    media: MediaKind,
    role: GrantRole,
) -> Result<CapabilityId, ConnectError> {
    catalog
        .match_endpoint(node, media, role)
        .map(|c: &Capability| c.id.clone())
        .ok_or_else(|| ConnectError::NoEndpoint {
            node: node.as_str().to_string(),
            media: media.token(),
            role: role.label(),
        })
}

fn codecs(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::{mobile_capabilities, MobileScope};
    use allmystuff_graph::{Flow, MeshNode, NodeKind, Relationship};

    /// A catalog with the phone (viewer/controller) and a desk PC I own that
    /// exposes a screen, a camera, and system audio.
    fn fleet_catalog(phone: &NodeId, desk: &NodeId) -> Catalog {
        let mut cat = Catalog::new();
        cat.nodes.push(MeshNode {
            id: phone.clone(),
            label: "Phone".into(),
            kind: NodeKind::This,
            relationship: Relationship::Mine,
            online: true,
        });
        cat.nodes.push(MeshNode {
            id: desk.clone(),
            label: "Desk PC".into(),
            kind: NodeKind::Machine,
            relationship: Relationship::Mine,
            online: true,
        });
        for c in mobile_capabilities(phone, MobileScope::ViewerController) {
            cat.capabilities.push(c);
        }
        let d = desk.as_str();
        cat.capabilities.push(Capability::new(
            desk.clone(),
            format!("{d}:screen"),
            "Screen",
            MediaKind::Display,
            Flow::Source,
            "screen",
        ));
        cat.capabilities.push(Capability::new(
            desk.clone(),
            format!("{d}:webcam"),
            "Webcam",
            MediaKind::Video,
            Flow::Source,
            "camera",
        ));
        cat.capabilities.push(Capability::new(
            desk.clone(),
            format!("{d}:system-audio"),
            "System audio",
            MediaKind::Audio,
            Flow::Duplex,
            "system",
        ));
        cat
    }

    fn offer(msg: &ControlMessage) -> serde_json::Value {
        serde_json::to_value(msg).unwrap()
    }

    #[test]
    fn screen_offer_wires_remote_display_to_the_phones_display_sink() {
        let phone = NodeId::from("phone");
        let desk = NodeId::from("desk");
        let cat = fleet_catalog(&phone, &desk);

        let msg = offer_screen(&cat, &desk, &phone).expect("authorized");
        let j = offer(&msg);
        assert_eq!(j["t"], "route");
        assert_eq!(j["kind"], "offer");
        assert_eq!(j["route"]["from"], "desk:screen");
        assert_eq!(j["route"]["to"], "phone:display-in");
        assert_eq!(j["route"]["media"], "display");
        assert_eq!(j["route"]["id"], "route:desk:screen→phone:display-in");
        assert_eq!(j["video"][0], "h264");
        // A display offer carries no audio codec list.
        assert!(j["audio"].as_array().map(|a| a.is_empty()).unwrap_or(true));
    }

    #[test]
    fn camera_offer_lands_on_video_in_not_display_in() {
        let phone = NodeId::from("phone");
        let desk = NodeId::from("desk");
        let cat = fleet_catalog(&phone, &desk);

        let j = offer(&offer_camera(&cat, &desk, &phone).unwrap());
        assert_eq!(j["route"]["from"], "desk:webcam");
        assert_eq!(j["route"]["to"], "phone:video-in");
        assert_eq!(j["route"]["media"], "video");
    }

    #[test]
    fn audio_offer_carries_opus() {
        let phone = NodeId::from("phone");
        let desk = NodeId::from("desk");
        let cat = fleet_catalog(&phone, &desk);

        let j = offer(&offer_audio(&cat, &desk, &phone).unwrap());
        assert_eq!(j["route"]["media"], "audio");
        assert_eq!(j["route"]["to"], "phone:audio-out");
        assert_eq!(j["audio"][0], "opus");
    }

    #[test]
    fn missing_endpoint_is_an_error_not_a_panic() {
        let phone = NodeId::from("phone");
        let desk = NodeId::from("desk");
        // A bare catalog: the desk advertises nothing.
        let mut cat = Catalog::new();
        for c in mobile_capabilities(&phone, MobileScope::ViewerController) {
            cat.capabilities.push(c);
        }
        let err = offer_screen(&cat, &desk, &phone).unwrap_err();
        assert!(matches!(err, ConnectError::NoEndpoint { .. }));
    }

    #[test]
    fn terminal_offer_matches_the_amst_wire_shape() {
        let host = NodeId::from("desk");
        let me = NodeId::from("phone");
        let j = offer(&offer_terminal(&host, &me, None, "tab-1"));
        assert_eq!(j["t"], "route");
        assert_eq!(j["kind"], "offer");
        assert_eq!(j["route"]["from"], "desk:terminal");
        assert_eq!(j["route"]["to"], "phone:term-view:tab-1");
        assert_eq!(j["route"]["media"], "generic");
        assert_eq!(
            j["route"]["id"],
            "route:desk:terminal→phone:term-view:tab-1"
        );
        // A fresh shell omits the session key entirely (skip_serializing_if).
        assert!(j.get("session").is_none() || j["session"].is_null());
    }

    #[test]
    fn terminal_attach_threads_the_session_id() {
        let j = offer(&offer_terminal(
            &NodeId::from("desk"),
            &NodeId::from("phone"),
            Some("term-3".into()),
            "tab-2",
        ));
        assert_eq!(j["session"], "term-3");
    }

    #[test]
    fn files_offer_is_a_generic_files_view_route() {
        let j = offer(&offer_files(
            &NodeId::from("desk"),
            &NodeId::from("phone"),
            "br-1",
        ));
        assert_eq!(j["route"]["from"], "desk:files");
        assert_eq!(j["route"]["to"], "phone:files-view:br-1");
        assert_eq!(j["route"]["media"], "generic");
    }

    #[test]
    fn teardown_names_the_route() {
        let j = offer(&teardown("route:desk:terminal→phone:term-view:tab-1"));
        assert_eq!(j["kind"], "teardown");
        assert_eq!(j["route_id"], "route:desk:terminal→phone:term-view:tab-1");
    }
}
