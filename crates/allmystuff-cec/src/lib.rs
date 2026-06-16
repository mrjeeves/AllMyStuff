//! # allmystuff-cec
//!
//! The client half of the **CEC service** — Critical Error Computing's hosted
//! backend — that AllMyStuff (and the headless agent) talk to over HTTP.
//!
//! The free app needs none of this. An *optional* account unlocks the two
//! advertised services:
//!
//!  * **Concierge** — the *Ask-for-Help* button. A press opens a help session
//!    that's exposed to online CEC agents; one accepts and joins the customer's
//!    help room as the single **CEC Service** node.
//!  * **Private Line** — "a venue of your own": CEC-hosted signaling/STUN/TURN
//!    serving only the customer's devices.
//!
//! When a customer has CEC hardware *or* a service, the app stands up an
//! isolated `cec-customer-<hash>` mesh (see [`convention`]) on which the only
//! non-customer peer is the CEC Service node — agents live behind the backend,
//! never as mesh peers.
//!
//! ## Shape
//!
//!  * [`model`] — the contract types (the wire shapes; see `CONTRACT.md`).
//!  * [`convention`] — the few strings the app, agent, and server must agree on.
//!  * [`transport`] — the HTTP seam: a [`Transport`] trait, the real
//!    [`ReqwestTransport`](transport::ReqwestTransport), and (in [`mock`]) a
//!    socket-free [`MockTransport`](mock::MockTransport).
//!  * [`CecClient`] — one typed method per endpoint, generic over the transport.
//!  * [`mock`] — an in-memory reference backend, the source of truth the
//!    `allmystuff-cec-mock` server and every test share.
//!
//! Everything builds and tests with nothing heavier than `serde` + `reqwest`
//! (and `reqwest` is optional) — the same dependency discipline the rest of
//! the workspace keeps.

pub mod client;
pub mod convention;
pub mod mock;
pub mod model;
pub mod transport;

pub use client::CecClient;
pub use model::*;
pub use transport::{ApiRequest, ApiResponse, Method, Transport};

#[cfg(feature = "reqwest")]
pub use transport::ReqwestTransport;

/// Everything that can go wrong talking to the CEC backend.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The request never got a well-formed HTTP response (DNS, TLS, socket,
    /// timeout, …).
    #[error("transport error: {0}")]
    Transport(String),

    /// The backend answered with a non-2xx status.
    #[error("CEC backend error (HTTP {status}): {message}")]
    Api {
        status: u16,
        /// A machine-readable code when the backend supplied one
        /// (e.g. `bad_code`, `offline`, `not_agent`).
        code: Option<String>,
        message: String,
    },

    /// A response body didn't match the expected shape.
    #[error("decode error: {0}")]
    Decode(String),

    /// A call that requires a session was made while signed out.
    #[error("not signed in to a CEC account")]
    Unauthenticated,
}

impl Error {
    /// The backend's machine-readable error code, if any. Lets callers branch
    /// on `bad_code` vs `offline` without string-matching the message.
    pub fn code(&self) -> Option<&str> {
        match self {
            Error::Api { code, .. } => code.as_deref(),
            _ => None,
        }
    }

    /// Whether this looks like an expired/invalid session the UI should treat
    /// as "signed out".
    pub fn is_auth(&self) -> bool {
        matches!(self, Error::Unauthenticated)
            || matches!(
                self,
                Error::Api { status: 401, .. } | Error::Api { status: 403, .. }
            )
    }
}

pub type Result<T> = std::result::Result<T, Error>;
