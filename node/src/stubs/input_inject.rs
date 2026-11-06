//! No-op twin of [`crate::input_inject`] for capture-less builds
//! (`--no-default-features`, i.e. iOS — see the `host` feature in
//! `Cargo.toml`).
//!
//! There is nothing to inject into: iOS has no synthetic-input API for an
//! app to drive its host device. Events that reach a route here are dropped
//! after the mesh's ownership gates — same call sites, no `enigo`, no
//! thread.

use std::sync::Arc;

use allmystuff_session::InputAction;

#[derive(Default)]
pub struct Injector {}

impl Injector {
    pub fn new() -> Self {
        Self::default()
    }

    /// There is no host input lifetime to allocate in this build.
    pub fn begin_route(&self, _route: &str) {}

    /// Report the unavailable host input plane to the route's controller.
    pub fn apply(
        &self,
        _route: &str,
        _peer: &str,
        _action: InputAction,
        on_error: Arc<dyn Fn(String, u64) + Send + Sync>,
    ) {
        on_error("this build cannot host keyboard or mouse control".into(), 0);
    }

    /// A capture-less build never owns an input backend generation.
    pub fn is_current(&self, _route: &str, _generation: u64) -> bool {
        false
    }

    /// Nothing is ever pressed, so nothing needs lifting.
    pub fn release_route(&self, _route: &str) {}
}
