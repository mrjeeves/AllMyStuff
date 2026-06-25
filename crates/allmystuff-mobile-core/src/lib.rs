//! # allmystuff-mobile-core
//!
//! The brain of the AllMyStuff **mobile** app (iOS / Android), minus
//! everything platform-specific. It is deliberately pure Rust — `serde`,
//! `thiserror`, and the three pure AllMyStuff library crates
//! ([`allmystuff_graph`], [`allmystuff_protocol`], [`allmystuff_session`]) —
//! so the bytes the phone puts on (and takes off) the mesh are verified by
//! `cargo test`, exactly the way the desktop's wire contract is. No webview,
//! no daemon, no native decoders, no network: those live one layer up.
//!
//! ## Why the phone is a *node*, not a thin client
//!
//! On the desktop, the GUI is a thin client of a separate `allmystuff-serve`
//! process it **spawns**, which in turn spawns the `myownmesh` daemon. iOS
//! forbids a sandboxed app from spawning child processes, so that model can't
//! cross over. The mobile app instead **embeds** `myownmesh-core` in-process
//! and becomes a first-class mesh peer: its own ed25519 identity, direct
//! WebRTC DTLS/SRTP to its peers, signaling only. The "no central server,
//! peer-to-peer, end-to-end encrypted" promise is preserved — see
//! `docs/MOBILE.md` for the full architecture and the cross-compile findings.
//!
//! That embedded engine is reached through one seam — [`MeshClient`] — which
//! the FFI layer over `myownmesh-core` implements on device and an in-memory
//! fake implements in tests. Everything in this crate is written against that
//! seam, so the orchestration logic is exercised without a radio.
//!
//! ## What a phone is, on the graph
//!
//! A phone is a **viewer / controller**, not a host (see [`MobileScope`]):
//!
//! * it can **receive** a remote machine's screen/camera ([`MediaKind::Video`]
//!   sink) and play its audio, so [`media::VideoSink`] reassembles MJPEG and
//!   the [`media::VideoDecoder`] seam feeds the platform H.264 decoder;
//! * it can **drive** a remote ([`MediaKind::Input`] source) — touch becomes
//!   normalized [`InputAction`]s via [`media::InputEncoder`];
//! * it can open a remote **shell** ([`media::TermPlane`]) and browse remote
//!   **files** ([`media::FileClient`]);
//! * it does **not** host its own screen, a PTY, or input injection. Those are
//!   desktop-only capture concerns with no clean (or, for input, any) mobile
//!   story. Phone-as-a-*source* (camera / mic / screen-share) is a later,
//!   opt-in [`MobileScope::ViewerControllerHost`] decision.
//!
//! ## Layout
//!
//! * [`caps`] — the phone's [`Capability`] set and advertised feature flags.
//! * [`node`] — assemble the phone's [`NodeProfile`] for presence.
//! * [`connect`] — build the [`RouteControl::Offer`] for a screen / terminal /
//!   files / audio route, validated through the receiver-side [`Catalog`].
//! * [`transport`] — the [`MeshClient`] seam and [`transport::classify`], which
//!   turns a raw `(channel, payload)` off the mesh into a typed [`Inbound`].
//! * [`media`] — the per-plane client pipelines (video, input, terminal,
//!   files), all of them pure functions over the [`allmystuff_session`] frame
//!   types.

pub mod caps;
pub mod connect;
pub mod media;
pub mod node;
pub mod transport;

pub use caps::{mobile_capabilities, MobileScope};
pub use connect::{offer_files, offer_screen, offer_terminal, ConnectError as OfferError};
pub use node::{mobile_profile, MobileNodeConfig};
pub use transport::{classify, Inbound, MeshClient, MeshError, MeshResult};

/// A convenient single import for the types a mobile front end touches most.
///
/// ```
/// use allmystuff_mobile_core::prelude::*;
///
/// let me = NodeId::from("phone-abc");
/// let caps = mobile_capabilities(&me, MobileScope::ViewerController);
/// assert!(caps.iter().any(|c| c.origin == "viewer"));
/// ```
pub mod prelude {
    pub use crate::caps::{mobile_capabilities, MobileScope};
    pub use crate::connect::{offer_files, offer_screen, offer_terminal};
    pub use crate::media::{
        FileClient, FileReply, InputEncoder, TermPlane, VideoDecoder, VideoSink, VideoUpdate,
    };
    pub use crate::node::{mobile_profile, MobileNodeConfig};
    pub use crate::transport::{classify, Inbound, MeshClient};

    pub use allmystuff_graph::{
        Capability, CapabilityId, Catalog, Flow, MediaKind, MeshNode, NodeId, NodeKind,
        Relationship, Route,
    };
    pub use allmystuff_protocol::{
        ControlMessage, NodeProfile, RouteControl, CHANNEL_CONTROL, CHANNEL_MEDIA, CHANNEL_PRESENCE,
    };
    pub use allmystuff_session::{
        FileEvent, InputAction, MediaPayload, TermEvent, VideoFrame, VideoStatusFrame,
    };
}
