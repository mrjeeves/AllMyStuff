//! No-op twin of [`crate::input_inject`] for capture-less builds
//! (`--no-default-features`, i.e. iOS — see the `host` feature in
//! `Cargo.toml`).
//!
//! There is nothing to inject into: iOS has no synthetic-input API for an
//! app to drive its host device. Events that reach a route here are dropped
//! after the mesh's ownership gates — same call sites, no `enigo`, no
//! thread.

use allmystuff_session::InputAction;

#[derive(Default)]
pub struct Injector {}

impl Injector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Dropped — no display server, no injection API, nothing held down.
    pub fn apply(&self, route: &str, _action: InputAction) {
        tracing::debug!("input event for {route} dropped: capture-less build");
    }

    /// Nothing is ever pressed, so nothing needs lifting.
    pub fn release_route(&self, _route: &str) {}
}
