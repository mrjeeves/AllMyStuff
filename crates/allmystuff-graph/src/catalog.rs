//! The [`Catalog`] — the whole graph in one place: which nodes exist,
//! what each can do, and what's wired to what. It's the single object the
//! UI reads and the only thing that mints a [`Route`], because route
//! creation is exactly where media, flow, and authorization get enforced.

use serde::{Deserialize, Serialize};

use crate::authz::{describe_action, describe_grant, Denied, GrantRequest};
use crate::model::*;

/// Everything the graph knows. Cheap to clone for a snapshot; the GUI
/// holds one and re-renders from it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Catalog {
    pub nodes: Vec<MeshNode>,
    pub capabilities: Vec<Capability>,
    pub routes: Vec<Route>,
}

/// Why a connection couldn't be made.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConnectError {
    #[error("unknown capability: {0}")]
    UnknownCapability(String),
    #[error("a capability can't connect to itself")]
    SelfLoop,
    #[error("{from} produces {from_media} but {to} expects {to_media}")]
    MediaMismatch {
        from: String,
        to: String,
        from_media: &'static str,
        to_media: &'static str,
    },
    #[error("{cap} can't act as a {wanted} ({label} is {flow})")]
    WrongFlow {
        cap: String,
        label: String,
        wanted: &'static str,
        flow: &'static str,
    },
    /// Boxed because [`Denied`] is by far the largest variant; boxing it
    /// keeps `Result<Route, ConnectError>` small on the happy path.
    #[error(transparent)]
    Denied(Box<Denied>),
}

impl From<Denied> for ConnectError {
    fn from(d: Denied) -> Self {
        ConnectError::Denied(Box::new(d))
    }
}

impl From<Box<Denied>> for ConnectError {
    fn from(d: Box<Denied>) -> Self {
        ConnectError::Denied(d)
    }
}

impl Catalog {
    pub fn new() -> Self {
        Self::default()
    }

    // ---- lookups ------------------------------------------------------

    pub fn node(&self, id: &NodeId) -> Option<&MeshNode> {
        self.nodes.iter().find(|n| &n.id == id)
    }

    pub fn capability(&self, id: &CapabilityId) -> Option<&Capability> {
        self.capabilities.iter().find(|c| &c.id == id)
    }

    /// Capabilities physically present on a node.
    pub fn capabilities_on<'a>(
        &'a self,
        node: &'a NodeId,
    ) -> impl Iterator<Item = &'a Capability> + 'a {
        self.capabilities.iter().filter(move |c| &c.node == node)
    }

    fn require_cap(&self, id: &CapabilityId) -> Result<&Capability, ConnectError> {
        self.capability(id)
            .ok_or_else(|| ConnectError::UnknownCapability(id.to_string()))
    }

    // ---- single-route creation ---------------------------------------

    /// Validate and authorize a connection from one capability to
    /// another, returning the [`Route`] it would create (not yet added to
    /// the catalog — call [`Catalog::add_route`] to commit it). This is
    /// the one place media, flow, and authorization are all checked.
    pub fn propose_route(
        &self,
        from: &CapabilityId,
        to: &CapabilityId,
    ) -> Result<Route, ConnectError> {
        let route = self.validate_route(from, to)?;
        self.authorize(&route)?;
        Ok(route)
    }

    /// Validate a connection for the **rooms plane**: every structural
    /// rule of [`Catalog::propose_route`] — known endpoints, media, flow —
    /// without the share-grant gate. Being in the same room *is* the
    /// consent, and it's scoped to the room session: the route lives only
    /// while the room toggle does, and no standing [`Grant`] is ever
    /// minted for it. A real virtual room — what happens there doesn't
    /// change what its members may do to each other outside it.
    pub fn propose_room_route(
        &self,
        from: &CapabilityId,
        to: &CapabilityId,
    ) -> Result<Route, ConnectError> {
        self.validate_route(from, to)
    }

    /// The structural half of a proposal (no authorization).
    fn validate_route(
        &self,
        from: &CapabilityId,
        to: &CapabilityId,
    ) -> Result<Route, ConnectError> {
        if from == to {
            return Err(ConnectError::SelfLoop);
        }
        let src = self.require_cap(from)?;
        let dst = self.require_cap(to)?;

        if !src.flow.can_source() {
            return Err(ConnectError::WrongFlow {
                cap: src.id.to_string(),
                label: src.label.clone(),
                wanted: "source",
                flow: flow_word(src.flow),
            });
        }
        if !dst.flow.can_sink() {
            return Err(ConnectError::WrongFlow {
                cap: dst.id.to_string(),
                label: dst.label.clone(),
                wanted: "sink",
                flow: flow_word(dst.flow),
            });
        }
        if !src.media.compatible(dst.media) {
            return Err(ConnectError::MediaMismatch {
                from: src.id.to_string(),
                to: dst.id.to_string(),
                from_media: src.media.label(),
                to_media: dst.media.label(),
            });
        }

        // Media for the route: the concrete one if either side pins it,
        // else Generic.
        let media = if src.media != MediaKind::Generic {
            src.media
        } else {
            dst.media
        };

        Ok(Route {
            id: route_id(from, to),
            from: from.clone(),
            to: to.clone(),
            media,
        })
    }

    /// Check a route against the relationships in play. Endpoints on your
    /// own nodes always pass; endpoints on a shared node need a grant for
    /// the role they're playing (the `from` side provides, the `to` side
    /// consumes).
    pub fn authorize(&self, route: &Route) -> Result<(), Box<Denied>> {
        self.check_endpoint(&route.from, route.media, GrantRole::Provide)?;
        self.check_endpoint(&route.to, route.media, GrantRole::Consume)?;
        Ok(())
    }

    fn check_endpoint(
        &self,
        cap_id: &CapabilityId,
        media: MediaKind,
        role: GrantRole,
    ) -> Result<(), Box<Denied>> {
        // A capability we can't find, or one on a node we don't model, is
        // treated as ours — there's no *shared* relationship to gate it.
        let Some(cap) = self.capability(cap_id) else {
            return Ok(());
        };
        let Some(node) = self.node(&cap.node) else {
            return Ok(());
        };
        let Some(share) = node.relationship.share() else {
            return Ok(()); // Mine — always allowed.
        };
        // A grant authorizes the *person*, not one machine: sharing with
        // someone lets them route the granted thing to any of their nodes.
        // So coverage is the union of grants across every node shared with
        // the same person — wherever the grant happened to be recorded.
        let permitted = self
            .nodes
            .iter()
            .filter_map(|n| n.relationship.share())
            .filter(|s| s.person.id == share.person.id)
            .flat_map(|s| s.grants.iter())
            .any(|g| g.permits(media, role, cap_id));
        if permitted {
            Ok(())
        } else {
            Err(Box::new(Denied {
                node: cap.node.clone(),
                person: share.person.id.clone(),
                person_name: share.person.name.clone(),
                media,
                role,
                capability: cap_id.clone(),
                action: describe_action(media, role),
            }))
        }
    }

    /// For a denied (or hypothetical) route, the minimal grant(s) that
    /// would authorize it — one per shared endpoint that's missing
    /// coverage. Powers the "Let {person} …?" one-tap share prompt.
    pub fn required_grants(&self, from: &CapabilityId, to: &CapabilityId) -> Vec<GrantRequest> {
        let media = self
            .capability(from)
            .map(|c| c.media)
            .unwrap_or(MediaKind::Generic);
        let mut out = Vec::new();
        for (cap_id, role) in [(from, GrantRole::Provide), (to, GrantRole::Consume)] {
            if let Err(denied) = self.check_endpoint(cap_id, media, role) {
                let denied = *denied;
                out.push(GrantRequest {
                    node: denied.node,
                    person: denied.person,
                    person_name: denied.person_name,
                    media: denied.media,
                    role: denied.role,
                    capability: Some(Grant::durable_capability(media, role, &denied.capability)),
                    description: if media == MediaKind::Display
                        && role == GrantRole::Provide
                        && Grant::durable_capability(media, role, &denied.capability)
                            .as_str()
                            .ends_with(":screen")
                    {
                        "See all screens on this machine".into()
                    } else {
                        describe_grant(media, role)
                    },
                });
            }
        }
        out
    }

    /// Commit a route to the catalog (idempotent on route id).
    pub fn add_route(&mut self, route: Route) {
        if !self.routes.iter().any(|r| r.id == route.id) {
            self.routes.push(route);
        }
    }

    /// Remove a route by id. Returns whether anything was removed.
    pub fn remove_route(&mut self, id: &str) -> bool {
        let before = self.routes.len();
        self.routes.retain(|r| r.id != id);
        self.routes.len() != before
    }

    // ---- endpoint matching ---------------------------------------------

    /// Pick the capability on `node` that can play `role` for `media` —
    /// where a plain "connect this to that machine" lands. Preference
    /// order: a synthetic machine capability (origin
    /// `screen`/`control`/`controller`/`system`) first, so a whole-computer
    /// flow lands on "this computer" rather than a stray device; then the
    /// category's **current default** device, so a plain "connect audio
    /// here" lands on the mic the machine actually uses; then the first
    /// physical match by id order.
    pub fn match_endpoint(
        &self,
        node: &NodeId,
        media: MediaKind,
        role: GrantRole,
    ) -> Option<&Capability> {
        // Clone the node id so the returned reference is tied to `&self`
        // alone, not to the (shorter-lived) `node` argument.
        let node = node.clone();
        let mut candidates: Vec<&Capability> = self
            .capabilities
            .iter()
            .filter(|c| c.node == node)
            .filter(|c| c.media.compatible(media))
            .filter(|c| match role {
                GrantRole::Provide => c.flow.can_source(),
                GrantRole::Consume => c.flow.can_sink(),
                GrantRole::Both => c.flow == Flow::Duplex,
            })
            .collect();
        candidates.sort_by(|a, b| {
            endpoint_rank(a)
                .cmp(&endpoint_rank(b))
                .then_with(|| a.id.0.cmp(&b.id.0))
        });
        candidates.into_iter().next()
    }
}

fn route_id(from: &CapabilityId, to: &CapabilityId) -> String {
    format!("route:{from}→{to}")
}

fn flow_word(f: Flow) -> &'static str {
    match f {
        Flow::Source => "a source",
        Flow::Sink => "a sink",
        Flow::Duplex => "duplex",
    }
}

fn is_machine_origin(origin: &str) -> bool {
    matches!(
        origin,
        "screen" | "control" | "controller" | "system" | "viewer" | "clipboard" | "storage-in"
    )
}

/// Sort key for [`Catalog::match_endpoint`] (lower = preferred): a
/// synthetic machine endpoint first, then the category's current default
/// device, then everything else. Ties broken by id at the call site.
fn endpoint_rank(c: &Capability) -> u8 {
    if is_machine_origin(&c.origin) {
        0
    } else if c.default {
        1
    } else {
        2
    }
}
