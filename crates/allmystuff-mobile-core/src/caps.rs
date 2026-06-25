//! The capabilities a phone advertises on the graph.
//!
//! This is the mobile counterpart to `allmystuff-bridge`'s
//! `capabilities_from_inventory`: where a desktop scans real hardware and
//! exposes a screen, a control sink, mics, monitors and so on, a phone has a
//! small, fixed, *synthetic* set that says "I am a thing you look through and
//! reach out with." The ids follow the same `<node>:<endpoint>` scheme the
//! bridge uses so a remote machine's [`Catalog`](allmystuff_graph::Catalog)
//! reasons about a phone exactly like any other node.
//!
//! The base (viewer / controller) set, on every phone:
//!
//! | id | media | flow | origin | why |
//! |---|---|---|---|---|
//! | `<n>:display-in` | Display | Sink | `remote-desktop` | a remote **screen** (RDC) lands here |
//! | `<n>:video-in` | Video | Sink | `viewer` | a remote **camera** lands here |
//! | `<n>:audio-out` | Audio | Sink | `speaker` | remote audio plays here |
//! | `<n>:keyboard-mouse` | Input | Source | `controller` | touch drives a remote |
//! | `<n>:clipboard` | Clipboard | Duplex | `clipboard` | cross-device copy/paste |
//!
//! The split between `display-in` and `video-in` is not cosmetic: the graph
//! routes a remote *screen* (`MediaKind::Display`) only to a display sink and
//! a remote *camera* (`MediaKind::Video`) only to a video sink — the same way
//! the desktop lands a remote screen on its physical monitor but a camera on
//! its synthetic `video-in`. A phone has no monitor to expose, so it exposes a
//! synthetic display sink that *means* "show a remote desktop on my screen."
//!
//! The host set ([`MobileScope::ViewerControllerHost`]) additionally lets the
//! phone be a *source* — its camera, its mic, and (where the platform allows)
//! its own screen. That pulls real capture work onto the device
//! (AVFoundation / Camera2 / ReplayKit / MediaProjection) and is therefore an
//! explicit opt-in, never the default.

use allmystuff_graph::{Capability, Flow, MediaKind, NodeId};
use allmystuff_protocol::{
    FEATURE_CAMERA, FEATURE_FILES, FEATURE_MEDIA_LANES, FEATURE_ROOMS, FEATURE_TERMINAL,
};

/// How much of itself a phone is willing to expose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MobileScope {
    /// The default and the only scope v1–v2 ship: the phone consumes (a
    /// remote screen, remote audio) and reaches out (drives input, opens a
    /// shell, browses files). It hosts nothing.
    ViewerController,
    /// Additionally offer the phone's own camera, microphone, and screen as
    /// *sources* to the fleet. Opt-in, because it pulls platform capture into
    /// the app. Builds on top of the viewer/controller set, never replaces it.
    ViewerControllerHost,
}

impl MobileScope {
    /// Does this scope let the phone act as a media *source*?
    pub fn is_host(self) -> bool {
        matches!(self, MobileScope::ViewerControllerHost)
    }
}

/// Build every capability a phone `node` should expose for `scope`.
///
/// The base set comes first and in a stable order (so a remote's
/// `match_endpoint` tie-breaks deterministically), then the host additions.
pub fn mobile_capabilities(node: &NodeId, scope: MobileScope) -> Vec<Capability> {
    let n = node.as_str();
    let mk = |id: String, label: &str, media, flow, origin: &str| {
        Capability::new(node.clone(), id, label, media, flow, origin.to_string())
    };

    let mut caps = vec![
        // Where a remote machine's *desktop* lands — the remote-control case.
        // `Display` media, so the graph wires a remote `screen` (Display
        // source) straight to it, exactly as the desktop lands one on its
        // physical monitor. A phone has no monitor to expose, so this synthetic
        // sink stands in for "render a remote desktop on my screen."
        mk(
            format!("{n}:display-in"),
            "Remote desktop",
            MediaKind::Display,
            Flow::Sink,
            "remote-desktop",
        ),
        // Where a remote *camera* lands. Same role as the desktop's synthetic
        // `video-in` — `Video` media, distinct from `Display` so a webcam feed
        // and a desktop stream never cross-wire.
        mk(
            format!("{n}:video-in"),
            "Camera viewer",
            MediaKind::Video,
            Flow::Sink,
            "viewer",
        ),
        // Where a remote machine's audio plays. The desktop folds this into a
        // duplex `system-audio` endpoint; a phone has nothing to capture in
        // that sense at viewer scope, so it advertises a plain sink.
        mk(
            format!("{n}:audio-out"),
            "Audio out",
            MediaKind::Audio,
            Flow::Sink,
            "speaker",
        ),
        // The outbound twin: the phone driving a remote. Mirrors the desktop's
        // `keyboard-mouse` *source* (origin `controller`) — what a console
        // forwards is "whatever this device's user does," which on a phone is
        // taps, drags, and the on-screen keyboard.
        mk(
            format!("{n}:keyboard-mouse"),
            "Touch control",
            MediaKind::Input,
            Flow::Source,
            "controller",
        ),
        // Cross-device clipboard. Duplex like the desktop's: the phone can both
        // push a copied value to a remote and pull one back.
        mk(
            format!("{n}:clipboard"),
            "Clipboard",
            MediaKind::Clipboard,
            Flow::Duplex,
            "clipboard",
        ),
    ];

    if scope.is_host() {
        caps.push(mk(
            format!("{n}:camera"),
            "Camera",
            MediaKind::Video,
            Flow::Source,
            "camera",
        ));
        caps.push(mk(
            format!("{n}:microphone"),
            "Microphone",
            MediaKind::Audio,
            Flow::Source,
            "microphone",
        ));
        // The phone's own screen as a remote display, for "show my phone on the
        // big screen." Platform-gated (ReplayKit / MediaProjection); only here
        // at host scope.
        caps.push(mk(
            format!("{n}:screen"),
            "Phone screen",
            MediaKind::Display,
            Flow::Source,
            "screen",
        ));
    }

    caps
}

/// The AllMyStuff feature tags a phone advertises in its
/// [`NodeProfile::features`](allmystuff_protocol::NodeProfile). A feature is
/// only ever offered to a peer that also advertises it, so listing one the
/// phone can drive (terminal, files, rooms) is what lets the *remote* offer it
/// back. `media-lanes` opts the phone into the RTP H.264/Opus track lanes
/// (the path the native decoder consumes). `camera` is host-scope only — the
/// phone won't claim it can source a camera unless it actually will.
pub fn mobile_features(scope: MobileScope) -> Vec<String> {
    let mut f = vec![
        FEATURE_TERMINAL.to_string(),
        FEATURE_FILES.to_string(),
        FEATURE_ROOMS.to_string(),
        FEATURE_MEDIA_LANES.to_string(),
    ];
    if scope.is_host() {
        f.push(FEATURE_CAMERA.to_string());
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(caps: &[Capability]) -> Vec<String> {
        caps.iter().map(|c| c.id.as_str().to_string()).collect()
    }

    #[test]
    fn viewer_controller_is_consumer_and_controller_only() {
        let me = NodeId::from("phone-1");
        let caps = mobile_capabilities(&me, MobileScope::ViewerController);
        let by_origin = |o: &str| caps.iter().find(|c| c.origin == o).cloned();

        let viewer = by_origin("viewer").expect("video-in");
        assert_eq!((viewer.media, viewer.flow), (MediaKind::Video, Flow::Sink));
        assert_eq!(viewer.id.as_str(), "phone-1:video-in");

        // The remote-desktop sink is Display, not Video — that's what lets a
        // remote screen land on the phone at all.
        let rdc = by_origin("remote-desktop").expect("display-in");
        assert_eq!((rdc.media, rdc.flow), (MediaKind::Display, Flow::Sink));
        assert_eq!(rdc.id.as_str(), "phone-1:display-in");

        let ctl = by_origin("controller").expect("keyboard-mouse");
        assert_eq!((ctl.media, ctl.flow), (MediaKind::Input, Flow::Source));
        assert_eq!(ctl.id.as_str(), "phone-1:keyboard-mouse");

        let spk = by_origin("speaker").expect("audio-out");
        assert_eq!((spk.media, spk.flow), (MediaKind::Audio, Flow::Sink));

        let clip = by_origin("clipboard").expect("clipboard");
        assert_eq!(
            (clip.media, clip.flow),
            (MediaKind::Clipboard, Flow::Duplex)
        );

        // A viewer phone never *hosts* a screen, camera, or mic: nothing it
        // exposes can be the `from` side of a video/display/audio route. (It
        // can still source input and clipboard — touch drives a remote, and
        // the clipboard is duplex.)
        assert!(
            caps.iter()
                .filter(|c| c.flow.can_source())
                .all(|c| !matches!(
                    c.media,
                    MediaKind::Video | MediaKind::Display | MediaKind::Audio
                )),
            "a viewer phone must not source video/display/audio"
        );
    }

    #[test]
    fn host_scope_adds_sources_on_top_of_the_base_set() {
        let me = NodeId::from("phone-1");
        let base = mobile_capabilities(&me, MobileScope::ViewerController);
        let host = mobile_capabilities(&me, MobileScope::ViewerControllerHost);

        // Host is a strict superset: every base id is still present.
        for id in ids(&base) {
            assert!(ids(&host).contains(&id), "host dropped base cap {id}");
        }

        let cam = host.iter().find(|c| c.origin == "camera").expect("camera");
        assert_eq!((cam.media, cam.flow), (MediaKind::Video, Flow::Source));
        assert!(host.iter().any(|c| c.origin == "screen"));
        assert!(host.iter().any(|c| c.origin == "microphone"));
        // ...and none of those exist at viewer scope.
        assert!(base.iter().all(|c| c.origin != "camera"));
    }

    #[test]
    fn features_gate_camera_behind_host_scope() {
        assert!(
            !mobile_features(MobileScope::ViewerController).contains(&FEATURE_CAMERA.to_string())
        );
        assert!(mobile_features(MobileScope::ViewerControllerHost)
            .contains(&FEATURE_CAMERA.to_string()));
        // Terminal + files are always offered — the phone can drive both.
        assert!(
            mobile_features(MobileScope::ViewerController).contains(&FEATURE_TERMINAL.to_string())
        );
        assert!(mobile_features(MobileScope::ViewerController).contains(&FEATURE_FILES.to_string()));
    }
}
