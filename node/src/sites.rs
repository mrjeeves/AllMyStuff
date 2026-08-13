//! The sites plane — AllMyStuff's reverse proxy.
//!
//! A *site* is a TCP service a machine is listening on (a local web app, a
//! database) that its owner chose to expose. This plane carries it across
//! the mesh so another of your machines can reach it through a locally-mapped
//! port — direct (the same number) when free, else remapped.
//!
//! Like [`crate::files::FilesPlane`], this struct is **state only**; the
//! [`crate::mesh::Mesh`] owns the async tasks (it captures the `Arc<Mesh>`
//! they need to send mesh frames). Two halves:
//!
//!  * **Host** (the machine the service runs on): on a [`SiteEvent::Open`]
//!    it connects to `127.0.0.1:<port>` — but only after re-checking the
//!    port is one it *currently advertises* ([`SitesProxy::is_port_exposed`]),
//!    the load-bearing control, so a peer can't pivot to an unexposed local
//!    service — and pumps bytes both ways.
//!  * **Client** (the machine reaching it): a local `TcpListener` accepts
//!    connections on the mapped port; each becomes one tunneled `conn`.
//!
//! One mesh route per (host, site) multiplexes every connection by `conn`
//! id. The byte transport is the JSON media channel (base64 like the files
//! plane), so this is for light/occasional access, not bulk throughput.
//!
//! It's a **transparent layer-4 tunnel** — raw bytes, no HTTP parsing, no
//! idle timeout, and each connection's read and write directions run as
//! independent tasks (full duplex). So it isn't limited to request/response
//! HTTP: a connection that the client upgrades to a **WebSocket** (or that
//! speaks Server-Sent Events, HTTP keep-alive, gRPC, SSH, a database wire
//! protocol…) keeps flowing both ways for its whole life, exactly as it
//! would direct. The proxy never interprets the stream; it just carries it.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::PathBuf;

use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Most simultaneous tunneled connections one site route will carry. A
/// browser opens several at once; this bounds a hostile or runaway peer to a
/// finite table. Further `Open`s are refused (the connection simply fails on
/// the client, exactly like a busy server).
pub const MAX_CONNS_PER_ROUTE: usize = 64;

/// Inbound frames buffered per connection before the local socket write must
/// catch up. Beyond it the connection is *reset* (never corrupted or grown
/// unbounded) — a TCP client just reconnects.
pub const CONN_QUEUE: usize = 256;

/// One live tunneled connection's handles. Dropping `tx` closes the inbound
/// channel, which ends the socket-*writer* task; aborting `read_handle` ends
/// the socket-*reader* task — together they tear one connection down on
/// either side. The connection is registered (with `tx`) the moment it
/// opens, so inbound `Data` that beats the socket wiring is *buffered* in
/// the channel, never dropped; `read_handle` is attached once the reader
/// task exists (immediately on the client, post-`connect` on the host).
struct ConnHandle {
    /// Inbound bytes for this connection → its local socket-writer task.
    tx: mpsc::Sender<Vec<u8>>,
    /// The socket→mesh reader task, aborted when the connection closes.
    /// `None` only during the brief window before the reader is attached.
    read_handle: Option<JoinHandle<()>>,
}

/// A site this machine has mapped to a local port — the client-side binding.
pub struct ClientMapping {
    /// The host node (canonical/display id) the site lives on.
    pub node: String,
    /// The host's port (what it listens on).
    pub host_port: u16,
    /// The local port this machine bound the tunnel on.
    pub local_port: u16,
    /// The accept loop for the local listener, aborted on unmap.
    accept_handle: JoinHandle<()>,
}

/// Persisted shape: the exposed services as id → display name (empty name =
/// use the scan's classified default). Additive + `#[serde(default)]` so an
/// older file (or none) loads as "nothing exposed".
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Persisted {
    #[serde(default)]
    exposed: BTreeMap<String, String>,
}

pub struct SitesProxy {
    /// The services this machine advertises — the opt-in exposed set, mapping
    /// each listening-service id (`tcp:8080`) to the display name to
    /// advertise it under (empty = the classified default). Persisted; empty
    /// by default, so nothing is shared until the owner says so.
    exposed: Mutex<BTreeMap<String, String>>,
    path: Option<PathBuf>,
    /// Every live tunneled connection, keyed by `(route_id, conn)`. Both
    /// host and client sides register here.
    conns: Mutex<HashMap<(String, u64), ConnHandle>>,
    /// Client-side mappings, keyed by the site route id.
    mappings: Mutex<HashMap<String, ClientMapping>>,
    /// Recently delivered site frames per route. The mesh daemon can briefly
    /// retain overlapping event subscriptions across reconnects, so one wire
    /// frame may be pushed more than once. Site transport is a byte stream:
    /// duplicate Data corrupts it and duplicate Open replaces a live conn.
    seen_frames: Mutex<HashMap<String, FrameWindow>>,
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
struct FrameKey {
    seq: u64,
    conn: u64,
    kind: u8,
}

#[derive(Default)]
struct FrameWindow {
    seen: HashSet<FrameKey>,
    order: VecDeque<FrameKey>,
}

const FRAME_DEDUP_WINDOW: usize = 512;

impl Default for SitesProxy {
    fn default() -> Self {
        Self::load()
    }
}

impl SitesProxy {
    /// Load the persisted exposed set from disk (empty when there's none).
    /// A corrupt file is quarantined aside and loads empty, loudly — empty
    /// is fail-safe here (nothing exposed), but it shouldn't be silent.
    pub fn load() -> Self {
        let path = store_path();
        let exposed = path
            .as_ref()
            .map(|p| crate::persist::load_json::<Persisted>(p).exposed)
            .unwrap_or_default();
        SitesProxy {
            exposed: Mutex::new(exposed),
            path,
            conns: Mutex::new(HashMap::new()),
            mappings: Mutex::new(HashMap::new()),
            seen_frames: Mutex::new(HashMap::new()),
        }
    }

    // ---- exposure (the host's allow-list) -----------------------------

    /// The exposed services as id → display name (the value is empty for one
    /// advertised under its classified default). Drives presence and the UI.
    pub fn exposed_map(&self) -> BTreeMap<String, String> {
        self.exposed.lock().clone()
    }

    /// Replace the exposed set (id → name) and persist it. Returns the new
    /// map.
    pub fn set_exposed(&self, map: BTreeMap<String, String>) -> BTreeMap<String, String> {
        let mut e = self.exposed.lock();
        *e = map;
        persist(&self.path, &e);
        e.clone()
    }

    /// Is `port` one this machine currently advertises? The host gate: the
    /// exposed id encodes the port (`tcp:<port>`), so this needs no scan —
    /// the client's claimed port is checked against *our own* exposed set.
    pub fn is_port_exposed(&self, port: u16) -> bool {
        self.exposed.lock().contains_key(&format!("tcp:{port}"))
    }

    // ---- connection table ---------------------------------------------

    /// Admit this frame once. The bounded arrival-order window tolerates
    /// interleaved connections without assuming sequence numbers are strictly
    /// ordered, and route teardown removes the whole incarnation's history.
    pub fn accept_frame(&self, route: &str, seq: u64, conn: u64, kind: u8) -> bool {
        let mut routes = self.seen_frames.lock();
        let window = routes.entry(route.to_string()).or_default();
        let key = FrameKey { seq, conn, kind };
        if !window.seen.insert(key) {
            return false;
        }
        window.order.push_back(key);
        if window.order.len() > FRAME_DEDUP_WINDOW {
            if let Some(oldest) = window.order.pop_front() {
                window.seen.remove(&oldest);
            }
        }
        true
    }

    /// Open a connection: if the route is under its [`MAX_CONNS_PER_ROUTE`]
    /// cap, register the inbound channel **now** and hand back its receiver
    /// (for the socket-writer task to drain). Registering up front is what
    /// lets [`Self::conn_tx`] accept inbound `Data` that arrives before the
    /// socket is wired (it buffers in the channel) — no first-bytes drop.
    /// `None` when the route is at its cap (the caller refuses the conn).
    pub fn open_conn(&self, route: &str, conn: u64) -> Option<mpsc::Receiver<Vec<u8>>> {
        let mut conns = self.conns.lock();
        if conns.keys().filter(|(r, _)| r == route).count() >= MAX_CONNS_PER_ROUTE {
            return None;
        }
        let (tx, rx) = mpsc::channel::<Vec<u8>>(CONN_QUEUE);
        conns.insert(
            (route.to_string(), conn),
            ConnHandle {
                tx,
                read_handle: None,
            },
        );
        Some(rx)
    }

    /// Attach the socket→mesh reader once it's spawned. If the connection was
    /// already closed in the meantime (a teardown during `connect`), the
    /// reader is aborted instead, so nothing is orphaned.
    pub fn attach_reader(&self, route: &str, conn: u64, read_handle: JoinHandle<()>) {
        match self.conns.lock().get_mut(&(route.to_string(), conn)) {
            Some(h) => h.read_handle = Some(read_handle),
            None => read_handle.abort(),
        }
    }

    /// The inbound-bytes sender for one connection, if it's still live.
    pub fn conn_tx(&self, route: &str, conn: u64) -> Option<mpsc::Sender<Vec<u8>>> {
        self.conns
            .lock()
            .get(&(route.to_string(), conn))
            .map(|h| h.tx.clone())
    }

    /// Close one connection: drop its inbound sender (the writer task's
    /// channel closes → it shuts the socket) and abort its reader. Idempotent.
    pub fn close_conn(&self, route: &str, conn: u64) {
        if let Some(h) = self.conns.lock().remove(&(route.to_string(), conn)) {
            if let Some(reader) = h.read_handle {
                reader.abort();
            }
        }
    }

    // ---- client-side mappings -----------------------------------------

    pub fn add_mapping(&self, route: String, mapping: ClientMapping) {
        self.mappings.lock().insert(route, mapping);
    }

    /// The route id mapping `(node, host_port)`, if this device has it mapped.
    /// Matched on the exact node id the UI passed (map and unmap both use the
    /// graph's id), so no canonical/display reconciliation is needed here.
    pub fn route_for(&self, node: &str, host_port: u16) -> Option<String> {
        self.mappings
            .lock()
            .iter()
            .find(|(_, m)| m.node == node && m.host_port == host_port)
            .map(|(route, _)| route.clone())
    }

    /// Whether the client mapping's accept task has already exited. A finished
    /// task means the local port is no longer listening even though the mapping
    /// record still exists — a failed/expired route offer must be rebuilt,
    /// never returned as healthy.
    pub fn mapping_task_finished(&self, route: &str) -> Option<bool> {
        self.mappings
            .lock()
            .get(route)
            .map(|mapping| mapping.accept_handle.is_finished())
    }

    /// The local ports already bound by this device — what
    /// [`allmystuff_bridge::sites::allocate_local_port`] avoids reusing.
    pub fn taken_local_ports(&self) -> BTreeSet<u16> {
        self.mappings
            .lock()
            .values()
            .map(|m| m.local_port)
            .collect()
    }

    /// Every live mapping as `(route_id, node, host_port, local_port)`.
    ///
    /// Keep the map key in this internal projection: callers doing lifecycle
    /// work must address the mapping by route id, not accidentally substitute
    /// the node id from the public three-field UI shape.
    pub fn list_mappings(&self) -> Vec<(String, String, u16, u16)> {
        self.mappings
            .lock()
            .iter()
            .map(|(route, m)| (route.clone(), m.node.clone(), m.host_port, m.local_port))
            .collect()
    }

    /// The `(node, host_port, local_port)` of the client mapping on `route`, if
    /// any. Captured before a teardown so a rejected route can be auto-re-mapped
    /// onto the *same* local port — healing an open `localhost:<port>` tab
    /// without a manual unmap/remap. Only client mappings live here, so a reject
    /// on a host-side or non-site route reads as `None`.
    pub fn mapping_details(&self, route: &str) -> Option<(String, u16, u16)> {
        self.mappings
            .lock()
            .get(route)
            .map(|m| (m.node.clone(), m.host_port, m.local_port))
    }

    /// Tear a site route down completely: abort its accept loop (if any) and
    /// close every connection it carried. Safe on either side, idempotent —
    /// called on unmap and on route teardown.
    pub fn stop_route(&self, route: &str) {
        self.seen_frames.lock().remove(route);
        if let Some(m) = self.mappings.lock().remove(route) {
            m.accept_handle.abort();
        }
        let drained: Vec<u64> = {
            let conns = self.conns.lock();
            conns
                .keys()
                .filter(|(r, _)| r == route)
                .map(|(_, c)| *c)
                .collect()
        };
        for conn in drained {
            self.close_conn(route, conn);
        }
    }
}

impl ClientMapping {
    pub fn new(
        node: String,
        host_port: u16,
        local_port: u16,
        accept_handle: JoinHandle<()>,
    ) -> Self {
        ClientMapping {
            node,
            host_port,
            local_port,
            accept_handle,
        }
    }
}

fn persist(path: &Option<PathBuf>, exposed: &BTreeMap<String, String>) -> bool {
    let Some(path) = path else { return true };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let persisted = Persisted {
        exposed: exposed.clone(),
    };
    match serde_json::to_string_pretty(&persisted) {
        Ok(json) => crate::persist::write_atomic(path, json.as_bytes()).is_ok(),
        Err(_) => false,
    }
}

/// `~/.myownmesh/allmystuff-sites.json`, honouring `MYOWNMESH_HOME` — the
/// same home the identity, ownership record, and networks store use.
fn store_path() -> Option<PathBuf> {
    Some(allmystuff_protocol::myownmesh_state_dir()?.join("allmystuff-sites.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposed_set_round_trips_and_gates_ports() {
        let proxy = SitesProxy {
            exposed: Mutex::new(BTreeMap::new()),
            path: None, // no disk in the test
            conns: Mutex::new(HashMap::new()),
            mappings: Mutex::new(HashMap::new()),
            seen_frames: Mutex::new(HashMap::new()),
        };
        assert!(proxy.exposed_map().is_empty());
        assert!(!proxy.is_port_exposed(8080));

        let map = proxy.set_exposed(BTreeMap::from([
            ("tcp:8080".to_string(), "My App".to_string()),
            ("tcp:5432".to_string(), String::new()),
        ]));
        assert_eq!(map.get("tcp:8080").map(String::as_str), Some("My App"));
        // The gate keys on the port encoded in the exposed id — no scan.
        assert!(proxy.is_port_exposed(8080));
        assert!(proxy.is_port_exposed(5432));
        assert!(!proxy.is_port_exposed(22), "never exposed → never proxied");
    }

    #[test]
    fn duplicate_frames_are_admitted_once_and_reset_with_route() {
        let proxy = SitesProxy {
            exposed: Mutex::new(BTreeMap::new()),
            path: None,
            conns: Mutex::new(HashMap::new()),
            mappings: Mutex::new(HashMap::new()),
            seen_frames: Mutex::new(HashMap::new()),
        };
        let route = "route:site";
        assert!(proxy.accept_frame(route, 7, 2, 1));
        assert!(!proxy.accept_frame(route, 7, 2, 1));
        // Same sequence on a different connection/event is not conflated.
        assert!(proxy.accept_frame(route, 7, 3, 2));
        proxy.stop_route(route);
        assert!(proxy.accept_frame(route, 7, 2, 1));
    }

    #[tokio::test]
    async fn mapping_task_marks_an_exited_accept_task_stale() {
        let proxy = SitesProxy {
            exposed: Mutex::new(BTreeMap::new()),
            path: None,
            conns: Mutex::new(HashMap::new()),
            mappings: Mutex::new(HashMap::new()),
            seen_frames: Mutex::new(HashMap::new()),
        };
        // Keep a finished handle in the mapping exactly like an offer that
        // expired before the host accepted it.
        let finished = tokio::spawn(async {});
        while !finished.is_finished() {
            tokio::task::yield_now().await;
        }
        proxy.add_mapping(
            "route:dead".into(),
            ClientMapping::new("peer-a".into(), 80, 47000, finished),
        );

        assert_eq!(proxy.mapping_task_finished("route:dead"), Some(true));
        assert_eq!(
            proxy.list_mappings(),
            vec![("route:dead".into(), "peer-a".into(), 80, 47000)]
        );
    }
}
