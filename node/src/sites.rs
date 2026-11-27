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

/// Restore a saved address on loopback. Give the previous listener time
/// to release the port after teardown.
pub(crate) async fn bind_exact_local_port(port: u16) -> Result<tokio::net::TcpListener, String> {
    use std::net::{Ipv4Addr, SocketAddr};
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut last = String::new();
    for _ in 0..20 {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => return Ok(listener),
            Err(error) => {
                last = error.to_string();
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
    Err(format!("local port :{port} still busy after 2s: {last}"))
}

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

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct SiteIntent {
    pub node: String,
    pub host_port: u16,
    pub local_port: u16,
    #[serde(skip)]
    pub remembered: bool,
}

impl SiteIntent {
    fn matches(&self, node: &str, port: u16) -> bool {
        crate::cec::pubkey_part(&self.node) == crate::cec::pubkey_part(node)
            && self.host_port == port
    }
}

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
struct Persisted {
    #[serde(default)]
    exposed: BTreeMap<String, String>,
    #[serde(default)]
    mappings: Vec<SiteIntent>,
}

pub struct SitesProxy {
    /// The services this machine advertises — the opt-in exposed set, mapping
    /// each listening-service id (`tcp:8080`) to the display name to
    /// advertise it under (empty = the classified default). Persisted; empty
    /// by default, so nothing is shared until the owner says so.
    settings: Mutex<Persisted>,
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
        Self::load_from(store_path())
    }

    fn load_from(path: Option<PathBuf>) -> Self {
        let mut settings = path
            .as_ref()
            .map(|p| crate::persist::load_json::<Persisted>(p))
            .unwrap_or_default();
        settings
            .mappings
            .retain(|m| !m.node.is_empty() && m.host_port != 0 && m.local_port != 0);
        for mapping in &mut settings.mappings {
            mapping.remembered = true;
        }
        SitesProxy {
            settings: Mutex::new(settings),
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
        self.settings.lock().exposed.clone()
    }

    /// Replace the exposed set (id → name) and persist it. Returns the new
    /// map.
    pub fn set_exposed(
        &self,
        exposed: BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, String>, String> {
        let mut settings = self.settings.lock();
        let next = Persisted {
            exposed,
            mappings: settings.mappings.clone(),
        };
        persist(&self.path, &next)?;
        *settings = next;
        Ok(settings.exposed.clone())
    }

    /// Is `port` one this machine currently advertises? The host gate: the
    /// exposed id encodes the port (`tcp:<port>`), so this needs no scan —
    /// the client's claimed port is checked against *our own* exposed set.
    pub fn is_port_exposed(&self, port: u16) -> bool {
        self.settings
            .lock()
            .exposed
            .contains_key(&format!("tcp:{port}"))
    }

    pub(crate) fn intents(&self) -> Vec<SiteIntent> {
        self.settings.lock().mappings.clone()
    }

    pub(crate) fn intent(&self, node: &str, port: u16) -> Option<SiteIntent> {
        self.settings
            .lock()
            .mappings
            .iter()
            .find(|m| m.matches(node, port))
            .cloned()
    }

    pub(crate) fn remember(&self, mut mapping: SiteIntent) -> Result<(), String> {
        let mut settings = self.settings.lock();
        let mut next = settings.clone();
        if let Some(current) = next
            .mappings
            .iter_mut()
            .find(|m| m.matches(&mapping.node, mapping.host_port))
        {
            mapping.remembered |= current.remembered;
            if *current == mapping {
                return Ok(());
            }
            *current = mapping.clone();
        } else {
            next.mappings.push(mapping.clone());
        }
        if mapping.remembered {
            persist(&self.path, &next)?;
        }
        *settings = next;
        Ok(())
    }

    pub(crate) fn forget(&self, node: &str, port: u16) -> Result<(), String> {
        let mut settings = self.settings.lock();
        let Some(index) = settings.mappings.iter().position(|m| m.matches(node, port)) else {
            return Ok(());
        };
        let mut next = settings.clone();
        if next.mappings.remove(index).remembered {
            persist(&self.path, &next)?;
        }
        *settings = next;
        Ok(())
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
    /// Saved intent uses a canonical id. The graph may use a display id.
    pub fn route_for(&self, node: &str, host_port: u16) -> Option<String> {
        self.mappings
            .lock()
            .iter()
            .find(|(_, m)| {
                crate::cec::pubkey_part(&m.node) == crate::cec::pubkey_part(node)
                    && m.host_port == host_port
            })
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
        let mut ports: BTreeSet<_> = self
            .mappings
            .lock()
            .values()
            .map(|m| m.local_port)
            .collect();
        ports.extend(self.settings.lock().mappings.iter().map(|m| m.local_port));
        ports
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

fn persist(path: &Option<PathBuf>, settings: &Persisted) -> Result<(), String> {
    let Some(path) = path else { return Ok(()) };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("couldn't save Sites settings: {e}"))?;
    }
    let persisted = Persisted {
        exposed: settings.exposed.clone(),
        mappings: settings
            .mappings
            .iter()
            .filter(|m| m.remembered)
            .cloned()
            .collect(),
    };
    let json = serde_json::to_vec_pretty(&persisted).map_err(|e| e.to_string())?;
    crate::persist::write_atomic(path, &json)
        .map_err(|e| format!("couldn't save Sites settings: {e}"))
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
        let proxy = SitesProxy::load_from(None);
        assert!(proxy.exposed_map().is_empty());
        assert!(!proxy.is_port_exposed(8080));

        let map = proxy
            .set_exposed(BTreeMap::from([
                ("tcp:8080".to_string(), "My App".to_string()),
                ("tcp:5432".to_string(), String::new()),
            ]))
            .unwrap();
        assert_eq!(map.get("tcp:8080").map(String::as_str), Some("My App"));
        // The gate keys on the port encoded in the exposed id — no scan.
        assert!(proxy.is_port_exposed(8080));
        assert!(proxy.is_port_exposed(5432));
        assert!(!proxy.is_port_exposed(22), "never exposed → never proxied");
    }

    #[test]
    fn duplicate_frames_are_admitted_once_and_reset_with_route() {
        let proxy = SitesProxy::load_from(None);
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
        let proxy = SitesProxy::load_from(None);
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
            proxy.mapping_details("route:dead"),
            Some(("peer-a".into(), 80, 47000))
        );
    }

    #[test]
    fn remembered_sites_and_exposure_survive_restart_and_forget_is_durable() {
        let dir = std::env::temp_dir().join(format!(
            "ams-sites-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("allmystuff-sites.json");
        std::fs::write(&path, br#"{"exposed":{"tcp:5432":"Resolve"}}"#).unwrap();
        let proxy = SitesProxy::load_from(Some(path.clone()));
        assert_eq!(proxy.exposed_map()["tcp:5432"], "Resolve");
        let resolve = SiteIntent {
            node: "server-abc12".into(),
            host_port: 5432,
            local_port: 5432,
            remembered: true,
        };
        proxy.remember(resolve.clone()).unwrap();
        proxy
            .remember(SiteIntent {
                node: "kvm".into(),
                host_port: 80,
                local_port: 47000,
                remembered: false,
            })
            .unwrap();
        proxy
            .set_exposed(BTreeMap::from([(
                "tcp:5432".into(),
                "DaVinci Resolve".into(),
            )]))
            .unwrap();
        drop(proxy);

        let restarted = SitesProxy::load_from(Some(path.clone()));
        assert_eq!(restarted.exposed_map()["tcp:5432"], "DaVinci Resolve");
        assert_eq!(restarted.intents(), vec![resolve]);
        assert_eq!(restarted.taken_local_ports(), BTreeSet::from([5432]));
        assert_eq!(restarted.route_for("server", 5432), None);
        restarted.forget("server", 5432).unwrap();
        let next = SitesProxy::load_from(Some(path.clone()));
        assert!(next.intent("server-abc12", 5432).is_none());
        assert!(next.is_port_exposed(5432));
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[tokio::test]
    async fn restoring_a_site_waits_for_the_same_loopback_port() {
        use std::time::Duration;
        let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = occupied.local_addr().unwrap();
        assert!(tokio::time::timeout(
            Duration::from_millis(200),
            bind_exact_local_port(address.port())
        )
        .await
        .is_err());
        drop(occupied);
        let restored = bind_exact_local_port(address.port()).await.unwrap();
        assert_eq!(restored.local_addr().unwrap(), address);
        let client = tokio::net::TcpStream::connect(address).await.unwrap();
        let (_, peer) = restored.accept().await.unwrap();
        assert_eq!(client.local_addr().unwrap(), peer);
    }
}
