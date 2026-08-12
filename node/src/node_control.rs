//! The node's own local control + event socket — the per-machine seam a thin
//! GUI drives instead of running its own in-process [`Mesh`].
//!
//! AllMyStuff is converging on **one node per machine** ([`crate::instance`]):
//! the headless `allmystuff-serve` binary owns the live [`Mesh`], and a future
//! GUI becomes a thin client that issues commands over *this* socket rather
//! than linking the engine and supervising its own daemon. This module is the
//! node side of that link — purely additive plumbing layered on top of the
//! engine, mirroring the shapes [`crate::control_client`] already uses to talk
//! to the `myownmesh` daemon:
//!
//!  * [`NodeClient::request`] / [`NodeClient::request_bytes`] — one short-lived
//!    round trip per command (like [`ControlClient::request`]).
//!  * [`NodeClient::subscribe_events`] — a long-lived stream of engine events
//!    (like [`ControlClient::subscribe_events`]).
//!  * [`serve`] — the accept loop the node runs, dispatching commands to its
//!    [`Mesh`] / [`ControlClient`] / [`DisabledNetworks`] and fanning engine
//!    events out to every subscribed client through a [`SocketSink`].
//!
//! The wire is **length-prefixed frames** (`[u32 BE len][1 tag byte][payload]`)
//! rather than the daemon's newline-JSON, because the poll commands and event
//! payloads carry raw binary (media batches) that newline framing can't.
//!
//! [`ControlClient`]: crate::control_client::ControlClient
//! [`ControlClient::request`]: crate::control_client::ControlClient::request
//! [`ControlClient::subscribe_events`]: crate::control_client::ControlClient::subscribe_events
//! [`Mesh`]: crate::mesh::Mesh

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use interprocess::local_socket::tokio::prelude::*;
use interprocess::local_socket::tokio::Listener;
#[cfg(unix)]
use interprocess::local_socket::GenericFilePath;
#[cfg(not(unix))]
use interprocess::local_socket::GenericNamespaced;
use interprocess::local_socket::ListenerOptions;
#[cfg(windows)]
use interprocess::os::windows::local_socket::ListenerOptionsExt as _;
#[cfg(windows)]
use interprocess::os::windows::security_descriptor::SecurityDescriptor;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, Mutex};

use allmystuff_graph::{Grant, NodeId, Person, PersonId};
use allmystuff_protocol::LOCAL_CLAIM_NETWORK_ID;
use allmystuff_session::{FileEvent, InputAction, TermEvent};

use crate::control_client::{ControlClient, Request};
use crate::mesh::Mesh;
use crate::networks_store::DisabledNetworks;
use crate::video_decode::DecoderPreference;
use crate::UiSink;

// ---------------------------------------------------------------------------
// Wire protocol
// ---------------------------------------------------------------------------

/// A JSON-bodied frame: a [`NodeRequest`] in, or a normal command's
/// `{ok,result,error}` response (and the event-stream ack) out.
pub const TAG_JSON: u8 = 0;
/// A raw-bytes frame: the response to a poll command (`video_poll`,
/// `term_poll`, `file_poll`), whose body is the engine's already length-framed
/// media batch — kept binary rather than re-encoded as JSON.
pub const TAG_BYTES: u8 = 1;
/// One streamed engine event on the long-lived event connection.
pub const TAG_EVENT: u8 = 2;
/// The "relaunch onto the staged update" signal, streamed on the event
/// connection just before the node re-execs.
pub const TAG_RESTART: u8 = 3;

/// The largest frame we'll read — a media batch poll can be sizeable, but a
/// length this far past anything legitimate is a desync or a hostile peer, and
/// allocating it would be the attack. 256 MiB is comfortably above any real
/// frame while still bounding the damage.
const MAX_FRAME_LEN: usize = 256 * 1024 * 1024;

/// Write one length-prefixed frame: `[u32 BE len][tag][payload]`, then flush.
/// `len` counts the tag byte plus the payload, so an empty payload is `len 1`.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    w: &mut W,
    tag: u8,
    payload: &[u8],
) -> std::io::Result<()> {
    let len = (payload.len() as u64 + 1) as u32;
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(&[tag]).await?;
    w.write_all(payload).await?;
    w.flush().await
}

/// Read one length-prefixed frame, returning `(tag, payload)`. `Ok(None)` is a
/// clean EOF *before any byte of a frame* — a peer that hung up between frames,
/// not a truncated one (a partial frame is an error). Rejects a length past
/// [`MAX_FRAME_LEN`] before allocating.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Option<(u8, Vec<u8>)>> {
    let mut len_buf = [0u8; 4];
    // A clean hangup right at a frame boundary is a normal end of stream.
    match r.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "zero-length frame (missing tag byte)",
        ));
    }
    if len > MAX_FRAME_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame length {len} exceeds the {MAX_FRAME_LEN}-byte ceiling"),
        ));
    }
    let mut tag = [0u8; 1];
    r.read_exact(&mut tag).await?;
    let mut payload = vec![0u8; len - 1];
    r.read_exact(&mut payload).await?;
    Ok(Some((tag[0], payload)))
}

// ---------------------------------------------------------------------------
// Request / response shapes
// ---------------------------------------------------------------------------

/// One command over the control socket: a command name plus its JSON args
/// object. Mirrors the GUI's Tauri command boundary — `cmd` is the command
/// name and `args` the (named) parameters as a JSON object.
#[derive(Debug, Serialize, Deserialize)]
pub struct NodeRequest {
    /// The command name (`scan_self`, `connect_route`, …) or the
    /// [`SUBSCRIBE_EVENTS`] sentinel.
    pub cmd: String,
    /// The command's named arguments, as a JSON object (empty for no-arg
    /// commands).
    #[serde(default)]
    pub args: Value,
}

/// The reserved `cmd` that turns a connection into the long-lived event
/// stream instead of a one-shot command.
pub const SUBSCRIBE_EVENTS: &str = "__subscribe_events";

/// One engine event as it travels the event connection — either an
/// `emit(event, payload)` from the [`UiSink`], or the relaunch signal.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NodeEvent {
    /// A `UiSink::emit` — one named event + its JSON payload.
    Emit { event: String, payload: Value },
    /// The node is re-execing onto a staged update (`UiSink::restart`).
    Restart,
}

/// What [`dispatch`] produces for one command: a JSON result, a raw-bytes
/// result (the poll commands), or an error string.
pub enum DispatchOut {
    /// A normal command's JSON result.
    Json(Value),
    /// A poll command's raw media batch.
    Bytes(Vec<u8>),
    /// The command failed; the string is surfaced to the client as `error`.
    Err(String),
}

// ---------------------------------------------------------------------------
// Socket addressing
// ---------------------------------------------------------------------------

/// Where the node's control socket lives — distinct from the `myownmesh`
/// daemon socket (this is AllMyStuff's own per-machine node, not the mesh
/// daemon). Mirrors [`crate::control_client`]'s addressing.
enum SocketAddr {
    #[cfg(unix)]
    Path(PathBuf),
    #[cfg(not(unix))]
    Name(String),
}

/// On Windows the namespaced pipe name. (On unix the socket is a file path
/// under the `~/.myownmesh` home; see [`node_socket_addr`].)
#[cfg(not(unix))]
const NODE_SOCKET_NAME: &str = "allmystuff-node";

/// Resolve the node control socket address. On unix it's
/// `<myownmesh_home>/allmystuff-node.sock` — the *same* `~/.myownmesh` home
/// (honoring `MYOWNMESH_HOME`) the ownership store and networks store use; on
/// Windows it's a namespaced pipe. Distinct from the daemon socket either way.
fn node_socket_addr() -> Result<SocketAddr> {
    #[cfg(unix)]
    {
        let home = allmystuff_protocol::myownmesh_state_dir()
            .context("resolve the ~/.myownmesh home for the node socket")?;
        Ok(SocketAddr::Path(home.join("allmystuff-node.sock")))
    }
    #[cfg(not(unix))]
    {
        Ok(SocketAddr::Name(NODE_SOCKET_NAME.to_string()))
    }
}

impl SocketAddr {
    /// This address as an interprocess [`Name`], for connect or bind.
    fn to_name(&self) -> Result<interprocess::local_socket::Name<'_>> {
        match self {
            #[cfg(unix)]
            SocketAddr::Path(p) => p
                .as_path()
                .to_fs_name::<GenericFilePath>()
                .context("node socket path → fs_name"),
            #[cfg(not(unix))]
            SocketAddr::Name(n) => n
                .as_str()
                .to_ns_name::<GenericNamespaced>()
                .context("node socket name → ns_name"),
        }
    }

    /// The on-disk socket file, on unix — for clearing a stale one before a
    /// fresh bind (daemons do the same).
    #[cfg(unix)]
    fn path(&self) -> &std::path::Path {
        match self {
            SocketAddr::Path(p) => p.as_path(),
        }
    }
}

// ---------------------------------------------------------------------------
// NodeClient — the GUI's (and the tests') side of the wire
// ---------------------------------------------------------------------------

/// Client of a running node's control socket. The GUI uses this in Phase B to
/// drive the node it no longer runs in-process; the tests use it to exercise
/// [`serve`]. Cheap to clone the address; every call opens its own connection
/// (a local round trip is cheap and pooling muddies node-restart semantics —
/// same reasoning as [`ControlClient`](crate::control_client::ControlClient)).
pub struct NodeClient {
    addr: SocketAddr,
}

impl NodeClient {
    /// Resolve the node socket address (does not connect).
    pub fn new() -> Result<Self> {
        Ok(Self {
            addr: node_socket_addr()?,
        })
    }

    async fn connect(&self) -> Result<LocalSocketStream> {
        let name = self.addr.to_name()?;
        LocalSocketStream::connect(name)
            .await
            .context("connect node socket — is `allmystuff-serve` running?")
    }

    /// One-shot command → JSON result. Opens a connection, writes one
    /// [`NodeRequest`] as a [`TAG_JSON`] frame, reads one `TAG_JSON` response,
    /// and returns its `result` (or errors with `error`).
    pub async fn request(&self, cmd: &str, args: Value) -> Result<Value> {
        let (tag, payload) = self.round_trip(cmd, args).await?;
        if tag != TAG_JSON {
            bail!("node sent a {tag} frame where a JSON response was expected");
        }
        let resp: WireResponse = serde_json::from_slice(&payload).context("parse node response")?;
        if resp.ok {
            Ok(resp.result)
        } else {
            Err(anyhow!(resp.error.unwrap_or_else(|| "(no error)".into())))
        }
    }

    /// One-shot command → raw bytes (the poll commands). Same as
    /// [`NodeClient::request`] but expects a [`TAG_BYTES`] response.
    pub async fn request_bytes(&self, cmd: &str, args: Value) -> Result<Vec<u8>> {
        let (tag, payload) = self.round_trip(cmd, args).await?;
        match tag {
            TAG_BYTES => Ok(payload),
            // A failed poll still comes back as a JSON error frame.
            TAG_JSON => {
                let resp: WireResponse =
                    serde_json::from_slice(&payload).context("parse node response")?;
                Err(anyhow!(resp.error.unwrap_or_else(|| {
                    "node returned JSON where bytes were expected".into()
                })))
            }
            other => bail!("node sent a {other} frame where raw bytes were expected"),
        }
    }

    /// Connect, send the request, read exactly one response frame, close.
    async fn round_trip(&self, cmd: &str, args: Value) -> Result<(u8, Vec<u8>)> {
        let stream = self.connect().await?;
        let (mut reader, mut writer) = stream.split();
        let body = serde_json::to_vec(&NodeRequest {
            cmd: cmd.to_string(),
            args,
        })?;
        write_frame(&mut writer, TAG_JSON, &body)
            .await
            .context("write node request")?;
        read_frame(&mut reader)
            .await
            .context("read node response")?
            .ok_or_else(|| anyhow!("node closed the connection without a response"))
    }

    /// Subscribe to the node's event stream: connect, send the subscribe
    /// sentinel, await the ack, then spawn a read loop forwarding each
    /// [`NodeEvent`] to `tx` until EOF. Returns once the ack lands.
    pub async fn subscribe_events(&self, tx: mpsc::Sender<NodeEvent>) -> Result<()> {
        let stream = self.connect().await?;
        let (mut reader, mut writer) = stream.split();
        let body = serde_json::to_vec(&NodeRequest {
            cmd: SUBSCRIBE_EVENTS.to_string(),
            args: Value::Null,
        })?;
        write_frame(&mut writer, TAG_JSON, &body)
            .await
            .context("write node subscribe")?;

        // The ack — a TAG_JSON `{ok:true}` — confirms we're registered.
        let (tag, payload) = read_frame(&mut reader)
            .await
            .context("read subscribe ack")?
            .ok_or_else(|| anyhow!("node closed the connection before the subscribe ack"))?;
        if tag != TAG_JSON {
            bail!("subscribe ack wasn't a JSON frame");
        }
        let ack: WireResponse = serde_json::from_slice(&payload).context("parse subscribe ack")?;
        if !ack.ok {
            return Err(anyhow!(
                "subscribe rejected: {}",
                ack.error.unwrap_or_else(|| "(no error)".into())
            ));
        }

        tokio::spawn(async move {
            // Keep the writer half alive for the read loop's lifetime.
            let _writer_keepalive = writer;
            loop {
                match read_frame(&mut reader).await {
                    Ok(Some((TAG_EVENT, body))) => {
                        match serde_json::from_slice::<NodeEvent>(&body) {
                            Ok(ev) => {
                                if tx.send(ev).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => tracing::warn!("malformed node event: {e}"),
                        }
                    }
                    Ok(Some((TAG_RESTART, _))) => {
                        let _ = tx.send(NodeEvent::Restart).await;
                        break;
                    }
                    Ok(Some((tag, _))) => {
                        tracing::warn!("unexpected node event frame tag {tag}");
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!("node event stream read failed: {e}");
                        break;
                    }
                }
            }
        });
        Ok(())
    }

    /// True when a node is already listening on the control socket.
    pub async fn probe() -> bool {
        let Ok(client) = NodeClient::new() else {
            return false;
        };
        client.connect().await.is_ok()
    }
}

/// The body of a normal (`TAG_JSON`) response frame.
#[derive(Debug, Serialize, Deserialize)]
struct WireResponse {
    ok: bool,
    #[serde(default)]
    result: Value,
    #[serde(default)]
    error: Option<String>,
}

impl WireResponse {
    fn ok(result: Value) -> Self {
        Self {
            ok: true,
            result,
            error: None,
        }
    }

    fn err(error: String) -> Self {
        Self {
            ok: false,
            result: Value::Null,
            error: Some(error),
        }
    }
}

// ---------------------------------------------------------------------------
// SocketSink — the node's UiSink, fanning events to every event connection
// ---------------------------------------------------------------------------

/// The subscribed event connections' senders — the fan-out task's registry,
/// shared with [`serve`]'s accept loop (each event connection pushes its
/// sender here, [`fan_out`] writes to them).
pub type Broadcaster = Arc<Mutex<Vec<mpsc::Sender<NodeEvent>>>>;

/// Build a fresh, empty broadcaster.
pub fn new_broadcaster() -> Broadcaster {
    Arc::new(Mutex::new(Vec::new()))
}

/// Create the ordered event hand-off: the sender a [`SocketSink`] pushes every
/// engine event into, and the receiver [`serve`]'s [`fan_out`] task drains.
/// Unbounded so `emit` (the [`UiSink`] contract is non-blocking) never stalls
/// the engine; FIFO so events reach subscribers in the order they happened.
pub fn event_channel() -> (
    mpsc::UnboundedSender<NodeEvent>,
    mpsc::UnboundedReceiver<NodeEvent>,
) {
    mpsc::unbounded_channel()
}

/// The node's [`UiSink`]: every engine event is both logged (via the wrapped
/// `inner` sink — the binary's `LogSink`) **and** handed to the fan-out task,
/// which streams it to every connected event subscriber, so a thin GUI sees
/// exactly what the headless node logs.
///
/// `restart` is delegated: the node binary owns re-exec, so [`SocketSink`]
/// signals a [`NodeEvent::Restart`] to subscribers (so a GUI can relaunch its
/// window) and then hands off to `inner.restart()`, which never returns.
pub struct SocketSink {
    /// The wrapped sink — the binary's `LogSink`, which owns re-exec.
    inner: Arc<dyn UiSink>,
    /// Ordered hand-off to the fan-out task. `emit` is called from many engine
    /// tasks at once; funnelling every event through one FIFO queue (rather
    /// than spawning a task per event, which the runtime may then reorder) is
    /// what keeps them in order on the wire — a stale session snapshot
    /// arriving *after* a newer one would mis-paint the GUI.
    tx: mpsc::UnboundedSender<NodeEvent>,
}

impl SocketSink {
    /// Wrap `inner` (the node binary's `LogSink`); events flow through `tx` to
    /// the fan-out task [`serve`] runs. Build the pair with [`event_channel`].
    pub fn new(inner: Arc<dyn UiSink>, tx: mpsc::UnboundedSender<NodeEvent>) -> Self {
        Self { inner, tx }
    }
}

impl UiSink for SocketSink {
    fn emit(&self, event: &str, payload: Value) {
        self.inner.emit(event, payload.clone());
        // Non-blocking + ordered: a dropped receiver (no fan-out running) just
        // discards, exactly like a UI with no listener.
        let _ = self.tx.send(NodeEvent::Emit {
            event: event.to_string(),
            payload,
        });
    }

    fn restart(&self) -> ! {
        // Tell subscribers to relaunch before we re-exec, give the fan-out a
        // beat to flush it, then delegate to the inner sink (re-execs, never
        // returns).
        let _ = self.tx.send(NodeEvent::Restart);
        std::thread::sleep(Duration::from_millis(100));
        self.inner.restart()
    }
}

/// Drain the ordered event queue and fan each event out to every subscribed
/// connection, in order. One task, one queue — so all subscribers observe
/// events in the same order the engine produced them. A subscriber whose buffer
/// is full loses the event (`try_send`, never block the fan-out) rather than
/// stalling every other subscriber; a disconnected one is reaped.
async fn fan_out(mut rx: mpsc::UnboundedReceiver<NodeEvent>, broadcaster: Broadcaster) {
    while let Some(ev) = rx.recv().await {
        let mut subs = broadcaster.lock().await;
        subs.retain(|tx| match tx.try_send(ev.clone()) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => true, // alive, just behind
            Err(mpsc::error::TrySendError::Closed(_)) => false, // gone — reap it
        });
    }
}

// ---------------------------------------------------------------------------
// The server
// ---------------------------------------------------------------------------

/// Bind the node control socket, enforcing **one node per machine**: the bind
/// itself is the guard (there is no separate lock). If a *live* node already
/// answers the socket, this machine is already served — the bind fails with an
/// error the caller treats as "step aside, don't start a second mesh". A
/// *stale* socket file (a crashed node) is cleared and the bind retried.
///
/// This is race-safe: two nodes starting at once both try to create the name;
/// the first wins and the second's create fails, probes the now-live winner,
/// and steps aside.
pub async fn bind_control_socket() -> Result<Listener> {
    let addr = node_socket_addr()?;
    match bind_owner_only(&addr) {
        Ok(listener) => Ok(listener),
        Err(_) => {
            // The name is taken. A node that answers owns the machine; a name
            // taken by nothing live is a corpse from a crash — clear it and
            // bind once more.
            if NodeClient::probe().await {
                bail!("another allmystuff node already owns this machine's control socket");
            }
            #[cfg(unix)]
            {
                let _ = std::fs::remove_file(addr.path());
            }
            bind_owner_only(&addr).context("bind the node control socket")
        }
    }
}

/// Bind the node control socket and, on Unix, restrict it to the owner. The
/// socket drives privileged operations (scan, route setup, terminal/files/
/// input), so only this user's processes may reach it.
///
/// We `chmod` the **bound socket file** to 0600 rather than use interprocess's
/// `mode()` option: that one `fchmod`s the socket *fd* before bind, which
/// macOS/BSD reject with `ENOTSUP` — failing the bind outright (no node starts).
/// A path `chmod` after bind works on every Unix; the window between bind and
/// chmod is a negligible startup-time TOCTOU. Without this the socket inherits
/// the umask — the audit's AMS-04 exposure (a same-host process reaching the
/// control API). On Windows the namespaced pipe uses interprocess's default
/// security descriptor; an owner-only DACL via `security_descriptor()` is a
/// documented follow-up.
fn bind_owner_only(addr: &SocketAddr) -> Result<Listener> {
    let options = ListenerOptions::new().name(addr.to_name()?);
    #[cfg(windows)]
    let options = if let Some(sid) = service_client_sid()? {
        // A SYSTEM-hosted node exposes administrator terminals, files and
        // input. Do not inherit the service account's default pipe DACL, and
        // do not grant every authenticated local user. The account that
        // installed the app is the only non-administrator client allowed.
        let sddl = format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{sid})");
        let sddl = widestring::U16CString::from_str(&sddl)
            .map_err(|e| anyhow!("invalid node pipe security descriptor: {e}"))?;
        let descriptor = SecurityDescriptor::deserialize(sddl.as_ucstr())
            .map_err(|e| anyhow!("couldn't create node pipe security descriptor: {e}"))?;
        options.security_descriptor(descriptor)
    } else {
        options
    };
    let listener = options.create_tokio()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) =
            std::fs::set_permissions(addr.path(), std::fs::Permissions::from_mode(0o600))
        {
            tracing::warn!(
                "couldn't restrict the node control socket to 0600 ({e}); \
                 it may be reachable by other local users"
            );
        }
    }
    Ok(listener)
}

#[cfg(windows)]
fn service_client_sid() -> Result<Option<String>> {
    let Some(sid) = std::env::var_os("ALLMYSTUFF_CLIENT_SID") else {
        return Ok(None);
    };
    let sid = sid.to_string_lossy().into_owned();
    let valid = sid.starts_with("S-1-")
        && sid
            .split('-')
            .skip(2)
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()));
    if !valid {
        bail!("refusing invalid ALLMYSTUFF_CLIENT_SID value");
    }
    Ok(Some(sid))
}

/// Accept connections on an already-bound `listener` forever, each on its own
/// task, and run the [`fan_out`] task that streams engine events to
/// subscribers. The first frame of every connection is a [`NodeRequest`]: the
/// [`SUBSCRIBE_EVENTS`] sentinel turns it into a long-lived event stream;
/// anything else is dispatched as a one-shot command and the connection closes
/// after its response.
pub async fn serve(
    listener: Listener,
    mesh: Arc<Mesh>,
    client: Arc<ControlClient>,
    disabled: Arc<DisabledNetworks>,
    broadcaster: Broadcaster,
    event_rx: mpsc::UnboundedReceiver<NodeEvent>,
) -> Result<()> {
    // Drain the engine's ordered event queue out to every subscribed client.
    tokio::spawn(fan_out(event_rx, broadcaster.clone()));
    tracing::info!("node control socket listening");

    loop {
        let stream = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("node control accept failed: {e}");
                continue;
            }
        };
        let mesh = mesh.clone();
        let client = client.clone();
        let disabled = disabled.clone();
        let broadcaster = broadcaster.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, mesh, client, disabled, broadcaster).await {
                tracing::debug!("node control connection ended: {e:#}");
            }
        });
    }
}

/// Serve one connection: read its first [`NodeRequest`], then either run the
/// event-writer loop (subscribe) or dispatch one command and reply.
async fn handle_connection(
    stream: LocalSocketStream,
    mesh: Arc<Mesh>,
    client: Arc<ControlClient>,
    disabled: Arc<DisabledNetworks>,
    broadcaster: Broadcaster,
) -> Result<()> {
    let (mut reader, mut writer) = stream.split();
    let Some((tag, body)) = read_frame(&mut reader).await? else {
        // Clean hangup before sending anything — nothing to do.
        return Ok(());
    };
    if tag != TAG_JSON {
        bail!("first node frame wasn't a JSON request (tag {tag})");
    }
    let req: NodeRequest = serde_json::from_slice(&body).context("parse node request")?;

    if req.cmd == SUBSCRIBE_EVENTS {
        return run_event_writer(writer, broadcaster).await;
    }

    let out = dispatch(&mesh, &client, &disabled, req).await;
    match out {
        DispatchOut::Json(v) => {
            let body = serde_json::to_vec(&WireResponse::ok(v))?;
            write_frame(&mut writer, TAG_JSON, &body).await?;
        }
        DispatchOut::Bytes(b) => {
            write_frame(&mut writer, TAG_BYTES, &b).await?;
        }
        DispatchOut::Err(e) => {
            let body = serde_json::to_vec(&WireResponse::err(e))?;
            write_frame(&mut writer, TAG_JSON, &body).await?;
        }
    }
    Ok(())
}

/// Register this connection in the broadcaster, ack, then drain its receiver
/// and write each [`NodeEvent`] as a frame until the client disconnects.
async fn run_event_writer<W: AsyncWrite + Unpin>(
    mut writer: W,
    broadcaster: Broadcaster,
) -> Result<()> {
    // A small buffer: an event-flooded-but-slow subscriber sheds load (the
    // sink's `try_send` drops) rather than growing memory without bound.
    let (tx, mut rx) = mpsc::channel::<NodeEvent>(256);
    broadcaster.lock().await.push(tx);

    let ack = serde_json::to_vec(&WireResponse::ok(Value::Null))?;
    write_frame(&mut writer, TAG_JSON, &ack)
        .await
        .context("write subscribe ack")?;

    while let Some(ev) = rx.recv().await {
        match ev {
            NodeEvent::Restart => {
                let _ = write_frame(&mut writer, TAG_RESTART, &[]).await;
                break;
            }
            other => {
                let body = serde_json::to_vec(&other)?;
                if write_frame(&mut writer, TAG_EVENT, &body).await.is_err() {
                    // The client went away; let the dead sender be reaped on
                    // the next broadcast (`try_send`/`is_closed` retain check).
                    break;
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// dispatch — one command → DispatchOut
// ---------------------------------------------------------------------------

/// Pull a required arg from the request's args object, deserializing into `T`.
fn arg<T: serde::de::DeserializeOwned>(args: &Value, key: &str) -> Result<T, String> {
    let v = args
        .get(key)
        .ok_or_else(|| format!("missing argument: {key}"))?;
    serde_json::from_value(v.clone()).map_err(|e| format!("bad argument {key}: {e}"))
}

/// Pull an optional arg (absent or `null` → `None`).
fn opt<T: serde::de::DeserializeOwned>(args: &Value, key: &str) -> Result<Option<T>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => serde_json::from_value(v.clone())
            .map(Some)
            .map_err(|e| format!("bad argument {key}: {e}")),
    }
}

/// Map `Result<T, String>` from a Mesh method to a JSON-or-error `DispatchOut`,
/// serializing the success value.
fn json_result<T: Serialize>(r: Result<T, String>) -> DispatchOut {
    match r {
        Ok(v) => match serde_json::to_value(v) {
            Ok(j) => DispatchOut::Json(j),
            Err(e) => DispatchOut::Err(e.to_string()),
        },
        Err(e) => DispatchOut::Err(e),
    }
}

/// Run one command against the live node. Argument names mirror the GUI's
/// Tauri command parameters; return types mirror the underlying [`Mesh`] /
/// [`ControlClient`] / [`DisabledNetworks`] methods.
pub async fn dispatch(
    mesh: &Arc<Mesh>,
    client: &Arc<ControlClient>,
    disabled: &Arc<DisabledNetworks>,
    req: NodeRequest,
) -> DispatchOut {
    let a = &req.args;
    // A tiny helper to bail out of arg parsing into a DispatchOut::Err.
    macro_rules! try_arg {
        ($e:expr) => {
            match $e {
                Ok(v) => v,
                Err(e) => return DispatchOut::Err(e),
            }
        };
    }

    match req.cmd.as_str() {
        // ---- this machine ------------------------------------------------
        "scan_self" => {
            let me = mesh
                .resolve_local_id()
                .await
                .unwrap_or_else(|| "this".to_string());
            let node = NodeId::from(me.as_str());
            let inv = allmystuff_inventory::scan();
            DispatchOut::Json(json!({
                "node_id": me,
                "label": inv.host.hostname,
                "hostname": inv.host.hostname,
                "summary": allmystuff_bridge::node_summary(&inv),
                "capabilities": allmystuff_bridge::capabilities_with_screens(
                    &inv,
                    &node,
                    &crate::video::extra_screens(),
                ),
            }))
        }

        // The CEC Support app's spec card: this machine's headline hardware
        // (CPU / RAM / GPUs / disks / temps) straight off a fresh scan — a
        // slice, not the whole inventory, because the customer app needs a
        // spec sheet, not the USB bus. Temps are whatever the OS exposes —
        // often nothing on consumer Windows boards — and the card hides the
        // row rather than invent readings.
        "machine_specs" => {
            let inv = allmystuff_inventory::scan();
            DispatchOut::Json(json!({
                "hostname": inv.host.hostname,
                "os": match &inv.host.os_version {
                    Some(v) => format!("{} {}", inv.host.os, v),
                    None => inv.host.os.clone(),
                },
                // The system's board field, verbatim (Linux `board_name`,
                // Windows `Win32_BaseBoard.Product`, macOS `hw.model`) —
                // no parsing or formatting; null when the platform doesn't
                // report one.
                "board": inv.host.board,
                // Just the product / model name — the DMI *product* field
                // without its maker prefix ("XPS 15", not "Dell Inc. XPS
                // 15"). The spec card shows THIS as the machine's identity;
                // the maker name doesn't tell a technician which box it is.
                "product": inv.host.product,
                "cpu": {
                    "brand": inv.cpu.brand,
                    "cores": inv.cpu.physical_cores,
                    "threads": inv.cpu.logical_cores,
                    "max_mhz": inv.cpu.max_mhz,
                },
                "memory": {
                    "total_bytes": inv.memory.total_bytes,
                    "available_bytes": inv.memory.available_bytes,
                },
                "gpus": inv
                    .gpus
                    .iter()
                    .map(|g| {
                        json!({
                            "name": g.name,
                            "vram_bytes": g.vram_bytes,
                        })
                    })
                    .collect::<Vec<_>>(),
                "disks": inv
                    .storage
                    .iter()
                    .map(|s| {
                        json!({
                            "name": s.name,
                            "mount": s.mount_point,
                            "total_bytes": s.total_bytes,
                            "available_bytes": s.available_bytes,
                            "removable": s.removable,
                        })
                    })
                    .collect::<Vec<_>>(),
                "temps": inv
                    .temps
                    .iter()
                    .map(|t| {
                        json!({
                            "label": t.label,
                            "celsius": t.celsius,
                        })
                    })
                    .collect::<Vec<_>>(),
            }))
        }

        // Temps alone, off the sensor read only — no PowerShell probes, no
        // full scan — so a UI can keep the spec card's one moving number
        // moving with a cheap poll while `machine_specs` stays one-shot.
        "machine_temps" => DispatchOut::Json(json!({
            "temps": allmystuff_inventory::temps()
                .iter()
                .map(|t| {
                    json!({
                        "label": t.label,
                        "celsius": t.celsius,
                    })
                })
                .collect::<Vec<_>>(),
        })),

        // ---- live mesh (presence + routing) ------------------------------
        "connect_route" => {
            let from: String = try_arg!(arg(a, "from"));
            let to: String = try_arg!(arg(a, "to"));
            let media: String = try_arg!(arg(a, "media"));
            let video: Option<Vec<String>> = try_arg!(opt(a, "video"));
            let session: Option<String> = try_arg!(opt(a, "session"));
            let room: Option<String> = try_arg!(opt(a, "room"));
            json_result(
                mesh.connect_term_scoped(from, to, media, video.unwrap_or_default(), session, room)
                    .await,
            )
        }
        "drive_map" => {
            let target: String = try_arg!(arg(a, "target"));
            let root: String = try_arg!(arg(a, "root"));
            let label: String = try_arg!(arg(a, "label"));
            let mount: String = try_arg!(arg(a, "mount"));
            json_result(mesh.drive_map(target, root, label, mount).await)
        }
        "drive_map_from" => {
            let source: String = try_arg!(arg(a, "source"));
            let root: String = try_arg!(arg(a, "root"));
            let label: String = try_arg!(arg(a, "label"));
            let mount: String = try_arg!(arg(a, "mount"));
            json_result(mesh.drive_map_from(source, root, label, mount).await)
        }
        "native_drives" => DispatchOut::Json(json!(mesh.native_drives())),
        // Shared folders — the file half of a person-to-person share. The
        // sharer mints an id for a folder (`folder_share`) and pins a grant to
        // its capability; the receiver opens it by that id at a mount point of
        // its own choosing (`folder_open`). No path ever crosses the wire.
        "folder_share" => {
            let path: String = try_arg!(arg(a, "path"));
            let label: String = a
                .get("label")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            json_result(mesh.folder_share(path, label))
        }
        "folder_share_from" => {
            let source: String = try_arg!(arg(a, "source"));
            let path: String = try_arg!(arg(a, "path"));
            let label: String = a
                .get("label")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            json_result(mesh.folder_share_from(source, path, label).await)
        }
        "folder_unshare" => {
            let id: String = try_arg!(arg(a, "id"));
            DispatchOut::Json(json!({ "removed": mesh.folder_unshare(id) }))
        }
        "folders" => DispatchOut::Json(mesh.folders()),
        "folder_open" => {
            let source: String = try_arg!(arg(a, "source"));
            let folder: String = try_arg!(arg(a, "folder"));
            let mount: String = a
                .get("mount")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            json_result(mesh.folder_open(source, folder, mount).await)
        }
        "folder_open_on" => {
            let target: String = try_arg!(arg(a, "target"));
            let source: String = try_arg!(arg(a, "source"));
            let folder: String = try_arg!(arg(a, "folder"));
            let mount: String = a
                .get("mount")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            json_result(
                mesh.folder_open_on(target, source, folder, mount)
                    .await,
            )
        }
        "kvm_media_stage" => {
            let source: String = try_arg!(arg(a, "source"));
            let kvm: String = try_arg!(arg(a, "kvm"));
            let path: String = try_arg!(arg(a, "path"));
            let label: String = try_arg!(arg(a, "label"));
            json_result(mesh.kvm_media_stage_from(source, kvm, path, label).await)
        }
        "kvm_media_unmount" => {
            let kvm: String = try_arg!(arg(a, "kvm"));
            json_result(mesh.kvm_media_unmount(kvm).await)
        }
        "disconnect_route" => {
            let route_id: String = try_arg!(arg(a, "route_id"));
            json_result(mesh.disconnect(route_id).await)
        }
        "claim_node" => {
            let node: String = try_arg!(arg(a, "node"));
            json_result(mesh.claim(node).await)
        }
        "upgrade_node" => {
            let node: String = try_arg!(arg(a, "node"));
            json_result(mesh.request_upgrade(node).await)
        }
        "restart_node" => {
            let node: String = try_arg!(arg(a, "node"));
            json_result(mesh.request_restart(node).await)
        }
        "restart_device" => {
            let node: String = try_arg!(arg(a, "node"));
            json_result(mesh.request_restart_device(node).await)
        }
        // The two halves of "app and node update together". An app that just
        // applied a self-update rewrote allmystuff-serve on disk, but a node it
        // did not spawn — the Always On service — is still executing the old
        // image; `node_version` is how the app notices, `restart_self` is how
        // it says so. Answered before the process goes (see Mesh::restart_self).
        "node_version" => DispatchOut::Json(serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
        })),
        "request_update" => {
            let minimum: String = try_arg!(arg(a, "minimum"));
            json_result(mesh.request_self_update(minimum).await)
        }
        "restart_self" => {
            mesh.restart_self();
            DispatchOut::Json(serde_json::json!({ "restarting": true }))
        }
        "link_status" => {
            // The engine's daemon-link status as last emitted — poll-safe
            // truth for a GUI that missed the one-shot subscription event.
            // (Distinct from "mesh_status" below, the raw daemon Status
            // passthrough.)
            let (status, error) = mesh.link_status();
            DispatchOut::Json(serde_json::json!({ "status": status, "error": error }))
        }
        "refresh_node" => {
            // `node` omitted / null = this device (re-scan + re-advertise).
            let node = a.get("node").and_then(|v| v.as_str()).map(str::to_string);
            json_result(mesh.refresh_node(node).await)
        }
        "set_claimable" => {
            let claimable: bool = try_arg!(arg(a, "claimable"));
            json_result(mesh.set_claimable(claimable).await)
        }
        "set_public_claims" => {
            // This device's public-mesh claiming policy — strictly local
            // (see `Mesh::set_public_claims`); there is deliberately no
            // remote/fleet-synced path to flip it.
            let on: bool = try_arg!(arg(a, "on"));
            json_result(mesh.set_public_claims(on).await)
        }
        "claim_via_code" => {
            // Remote claim by the code shown on the claimee device.
            let code: String = try_arg!(arg(a, "code"));
            json_result(mesh.claim_via_code(code).await)
        }
        "kvm_attach" => {
            let node: String = try_arg!(arg(a, "node"));
            let target: String = try_arg!(arg(a, "target"));
            json_result(mesh.kvm_attach(node, target).await)
        }
        "kvm_detach" => {
            let node: String = try_arg!(arg(a, "node"));
            json_result(mesh.kvm_detach(node).await)
        }
        "kvm_mesh_add" => {
            let node: String = try_arg!(arg(a, "node"));
            let network_id: String = try_arg!(arg(a, "network_id"));
            json_result(mesh.kvm_mesh_add(node, network_id).await)
        }
        "kvm_mesh_remove" => {
            let node: String = try_arg!(arg(a, "node"));
            let network_id: String = try_arg!(arg(a, "network_id"));
            json_result(mesh.kvm_mesh_remove(node, network_id).await)
        }

        // ---- shares ------------------------------------------------------
        "share_grant" => {
            let person: Person = try_arg!(arg(a, "person"));
            let node: String = try_arg!(arg(a, "node"));
            let grant: Grant = try_arg!(arg(a, "grant"));
            json_result(mesh.share_grant(person, node.into(), grant).await)
        }
        "share_revoke" => {
            let person: String = try_arg!(arg(a, "person"));
            let grant_id: String = try_arg!(arg(a, "grant_id"));
            json_result(mesh.share_revoke(PersonId::from(person), grant_id).await)
        }
        "share_stop" => {
            let person: String = try_arg!(arg(a, "person"));
            json_result(mesh.share_stop(PersonId::from(person)).await)
        }

        // ---- input + clipboard ------------------------------------------
        "send_input" => {
            let route_id: String = try_arg!(arg(a, "route_id"));
            let action: InputAction = try_arg!(arg(a, "action"));
            json_result(mesh.send_input(route_id, action).await)
        }
        "clipboard_paste" => {
            let route_id: String = try_arg!(arg(a, "route_id"));
            json_result(mesh.clipboard_paste(route_id).await)
        }
        "clipboard_drop" => {
            let route_id: String = try_arg!(arg(a, "route_id"));
            let paths: Vec<String> = try_arg!(arg(a, "paths"));
            json_result(mesh.clipboard_drop(route_id, paths).await)
        }
        "clipboard_pull" => {
            let route_id: String = try_arg!(arg(a, "route_id"));
            json_result(mesh.clipboard_pull(route_id).await)
        }

        // ---- video plane -------------------------------------------------
        "video_watch" => {
            let route_id: String = try_arg!(arg(a, "route_id"));
            let decode: Option<bool> = try_arg!(opt(a, "decode"));
            let decoder: Option<String> = try_arg!(opt(a, "decoder"));
            let decoder = try_arg!(DecoderPreference::parse(decoder.as_deref()));
            DispatchOut::Json(json!(mesh.video_watch(
                route_id,
                decode.unwrap_or(false),
                decoder,
            )))
        }
        "video_poll" => {
            let route_id: String = try_arg!(arg(a, "route_id"));
            DispatchOut::Bytes(mesh.video_poll(&route_id))
        }
        "video_unwatch" => {
            let route_id: String = try_arg!(arg(a, "route_id"));
            let token: u64 = try_arg!(arg(a, "token"));
            mesh.video_unwatch(&route_id, token);
            DispatchOut::Json(Value::Null)
        }
        "video_refresh" => {
            let route_id: String = try_arg!(arg(a, "route_id"));
            json_result(mesh.request_refresh(route_id).await)
        }
        "video_feedback" => {
            let route_id: String = try_arg!(arg(a, "route_id"));
            let recv_fps: u32 = try_arg!(arg(a, "recv_fps"));
            let decode_fails: u32 = try_arg!(arg(a, "decode_fails"));
            let queue_depth: u32 = try_arg!(arg(a, "queue_depth"));
            // The webview decode ladder can't name the failed AU (its
            // decoder is opaque); the native lane's glitch path reports
            // the timestamp itself.
            json_result(
                mesh.send_video_feedback(route_id, recv_fps, decode_fails, queue_depth, None)
                    .await,
            )
        }
        "tune_route" => {
            let route_id: String = try_arg!(arg(a, "route_id"));
            let max_edge: Option<u32> = try_arg!(opt(a, "max_edge"));
            let bitrate: Option<u32> = try_arg!(opt(a, "bitrate"));
            let fps: Option<u32> = try_arg!(opt(a, "fps"));
            let game: Option<bool> = try_arg!(opt(a, "game"));
            let mode: Option<String> = try_arg!(opt(a, "mode"));
            json_result(
                mesh.request_tune(
                    route_id,
                    max_edge,
                    bitrate,
                    fps,
                    game.unwrap_or(false),
                    mode,
                )
                .await,
            )
        }
        // GUI-internal, read-only: the effective encode dials for a route
        // this node streams — the console's "requested → effective" panel
        // polls it ~1 Hz while open. Null (not an error) when this node isn't
        // the streamer, so the viewer quietly falls back to its own measured
        // actuals. Not wire-visible.
        "route_dials" => {
            let route_id: String = try_arg!(arg(a, "route_id"));
            match mesh.route_dials(&route_id) {
                Some(d) => DispatchOut::Json(json!({
                    "posture": d.posture,
                    "encoderLabel": d.encoder_label,
                    "codec": d.codec,
                    "targetBitrateBps": d.target_bitrate_bps,
                    "ceilingBps": d.ceiling_bps,
                    "fpsTarget": d.fps_target,
                    "edgeCap": d.edge_cap,
                    "outW": d.out_w,
                    "outH": d.out_h,
                })),
                None => DispatchOut::Json(Value::Null),
            }
        }
        // The Mode dropdown's Experimental (Labs) toggle: flip the tier
        // gate this process reads. GUI-internal (never wire-visible); a
        // feature name flips one feature, its absence the whole tier.
        "labs_set" => {
            let on: bool = try_arg!(arg(a, "on"));
            let feature: Option<String> = try_arg!(opt(a, "feature"));
            match feature {
                Some(f) => crate::labs::set_feature(&f, on),
                None => crate::labs::set_tier(on),
            }
            json_result(Ok::<(), String>(()))
        }

        // ---- terminal plane ----------------------------------------------
        "term_send" => {
            let route_id: String = try_arg!(arg(a, "route_id"));
            let event: TermEvent = try_arg!(arg(a, "event"));
            json_result(mesh.term_send(route_id, event).await)
        }
        "term_watch" => {
            let route_id: String = try_arg!(arg(a, "route_id"));
            DispatchOut::Json(json!(mesh.term_watch(&route_id)))
        }
        "term_poll" => {
            let route_id: String = try_arg!(arg(a, "route_id"));
            DispatchOut::Bytes(mesh.term_poll(&route_id))
        }
        "term_unwatch" => {
            let route_id: String = try_arg!(arg(a, "route_id"));
            let token: u64 = try_arg!(arg(a, "token"));
            mesh.term_unwatch(&route_id, token);
            DispatchOut::Json(Value::Null)
        }
        "terminal_sessions" => {
            let node: String = try_arg!(arg(a, "node"));
            json_result(mesh.request_terminal_sessions(node).await)
        }

        // ---- files plane -------------------------------------------------
        "file_send" => {
            let route_id: String = try_arg!(arg(a, "route_id"));
            let event: FileEvent = try_arg!(arg(a, "event"));
            json_result(mesh.file_send(route_id, event).await)
        }
        "file_watch" => {
            let route_id: String = try_arg!(arg(a, "route_id"));
            DispatchOut::Json(json!(mesh.file_watch(&route_id)))
        }
        "file_poll" => {
            let route_id: String = try_arg!(arg(a, "route_id"));
            DispatchOut::Bytes(mesh.file_poll(&route_id))
        }
        "file_unwatch" => {
            let route_id: String = try_arg!(arg(a, "route_id"));
            let token: u64 = try_arg!(arg(a, "token"));
            mesh.file_unwatch(&route_id, token);
            DispatchOut::Json(Value::Null)
        }
        "file_download" => {
            let route_id: String = try_arg!(arg(a, "route_id"));
            let req_id: u64 = try_arg!(arg(a, "req"));
            let name: String = try_arg!(arg(a, "name"));
            json_result(mesh.file_download(route_id, req_id, &name))
        }

        // ---- sites (reverse proxy) ---------------------------------------
        "site_scan" => {
            let mesh = mesh.clone();
            match tokio::task::spawn_blocking(move || mesh.site_scan()).await {
                Ok(list) => json_result::<Vec<_>>(Ok(list)),
                Err(e) => DispatchOut::Err(e.to_string()),
            }
        }
        "site_exposed" => {
            json_result::<std::collections::BTreeMap<String, String>>(Ok(mesh.site_exposed()))
        }
        "site_set_exposed" => {
            let exposed: std::collections::BTreeMap<String, String> = try_arg!(arg(a, "exposed"));
            json_result::<std::collections::BTreeMap<String, String>>(Ok(mesh
                .site_set_exposed(exposed)
                .await))
        }
        "site_map" => {
            let node: String = try_arg!(arg(a, "node"));
            let port: u16 = try_arg!(arg(a, "port"));
            match mesh.site_map(node, port).await {
                Ok(local_port) => DispatchOut::Json(json!({ "localPort": local_port })),
                Err(e) => DispatchOut::Err(e),
            }
        }
        "site_unmap" => {
            let node: String = try_arg!(arg(a, "node"));
            let port: u16 = try_arg!(arg(a, "port"));
            json_result(mesh.site_unmap(node, port).await)
        }
        "site_mappings" => {
            let mappings: Vec<Value> = mesh
                .site_mappings()
                .into_iter()
                .map(|(node, port, local_port)| {
                    json!({ "node": node, "port": port, "localPort": local_port })
                })
                .collect();
            DispatchOut::Json(Value::Array(mappings))
        }
        "site_remote_list" => {
            let node: String = try_arg!(arg(a, "node"));
            json_result(mesh.site_remote_list(node).await)
        }
        "site_remote_set_exposed" => {
            let node: String = try_arg!(arg(a, "node"));
            let exposed: std::collections::BTreeMap<String, String> = try_arg!(arg(a, "exposed"));
            json_result(mesh.site_remote_set_exposed(node, exposed).await)
        }

        // ---- session + fleet + rooms -------------------------------------
        "session_snapshot" => DispatchOut::Json(mesh.snapshot()),
        "room_send" => {
            let members: Vec<String> = try_arg!(arg(a, "members"));
            let message: allmystuff_protocol::RoomMessage = try_arg!(arg(a, "message"));
            json_result(mesh.room_send(members, message).await)
        }
        "room_scope_set" => {
            let room: String = try_arg!(arg(a, "room"));
            let members: Vec<String> = try_arg!(arg(a, "members"));
            json_result(mesh.room_scope_set(room, members).await)
        }
        "room_scope_leave" => {
            let room: String = try_arg!(arg(a, "room"));
            json_result(mesh.room_scope_leave(room).await)
        }
        "room_share_files" => {
            let members: Vec<String> = try_arg!(arg(a, "members"));
            let paths: Vec<String> = try_arg!(arg(a, "paths"));
            json_result::<Vec<_>>(Ok(mesh.room_share_files(members, paths)))
        }
        "room_set_share_peers" => {
            let tokens: Vec<String> = try_arg!(arg(a, "tokens"));
            let members: Vec<String> = try_arg!(arg(a, "members"));
            mesh.room_set_share_peers(tokens, members);
            DispatchOut::Json(Value::Null)
        }
        "room_unshare" => {
            let tokens: Vec<String> = try_arg!(arg(a, "tokens"));
            mesh.room_unshare(tokens);
            DispatchOut::Json(Value::Null)
        }
        "owned_roster" => DispatchOut::Json(mesh.fleet_roster_value().await),
        "fleet_leave" => json_result(mesh.fleet_leave().await),
        "reset_networking" => json_result(mesh.reset_networking().await),
        "factory_reset" => json_result(mesh.factory_reset().await),
        "fleet_kick" => {
            let device: String = try_arg!(arg(a, "device"));
            let code: Option<String> = try_arg!(opt(a, "code"));
            json_result(mesh.fleet_kick(device, code).await)
        }
        "fleet_set_name" => {
            let name: String = try_arg!(arg(a, "name"));
            json_result(mesh.fleet_set_name(name).await)
        }
        "fleet_grant_role" => {
            let device: String = try_arg!(arg(a, "device"));
            let role: String = try_arg!(arg(a, "role"));
            let code: Option<String> = try_arg!(opt(a, "code"));
            json_result(mesh.fleet_grant_role(device, role, code).await)
        }
        "fleet_revoke_role" => {
            let device: String = try_arg!(arg(a, "device"));
            let code: Option<String> = try_arg!(opt(a, "code"));
            json_result(mesh.fleet_revoke_role(device, code).await)
        }
        "fleet_set_hubs" => {
            let hubs: Vec<String> = try_arg!(arg(a, "hubs"));
            let redundancy: Option<u32> = try_arg!(opt(a, "redundancy"));
            let code: Option<String> = try_arg!(opt(a, "code"));
            json_result(mesh.fleet_set_hubs(hubs, redundancy, code).await)
        }

        // ---- daemon passthroughs ----------------------------------------
        "mesh_status" => daemon_request(client, Request::Status).await,
        "mesh_identity" => daemon_request(client, Request::IdentityShow).await,
        "mesh_networks" => daemon_request(client, Request::NetworksList).await,
        "mesh_peers" => {
            let network: String = try_arg!(arg(a, "network"));
            daemon_request(client, Request::PeersList { network }).await
        }
        "mesh_config_show" => daemon_request(client, Request::ConfigShow).await,
        "mesh_roster_approve" => {
            let network: String = try_arg!(arg(a, "network"));
            let device_id: String = try_arg!(arg(a, "device_id"));
            let label: Option<String> = try_arg!(opt(a, "label"));
            daemon_request(
                client,
                Request::RosterApprove {
                    network,
                    device_id,
                    label,
                },
            )
            .await
        }
        "mesh_roster_remove" => {
            let network: String = try_arg!(arg(a, "network"));
            let device_id: String = try_arg!(arg(a, "device_id"));
            daemon_request(client, Request::RosterRemove { network, device_id }).await
        }
        "mesh_roster_list" => {
            let network: String = try_arg!(arg(a, "network"));
            daemon_request(client, Request::RosterList { network }).await
        }
        "mesh_network_id_generate" => daemon_request(client, Request::NetworkIdGenerate).await,
        "mesh_network_add" => {
            let config: Value = try_arg!(arg(a, "config"));
            sync_after(
                mesh,
                daemon_request(client, Request::NetworkAdd { config }).await,
            )
            .await
        }
        "mesh_network_update" => {
            let config: Value = try_arg!(arg(a, "config"));
            let target = config
                .get("network_id")
                .or_else(|| config.get("id"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            // The local claim network has no settings — it's the fixed mDNS
            // passthrough for claiming and local pairing (venue-less, LAN
            // only). Its config is node-owned; an edit could only break it.
            if target.as_deref() == Some(LOCAL_CLAIM_NETWORK_ID) {
                return DispatchOut::Err(
                    "the Local network has no settings to edit — it's the fixed \
                     mDNS passthrough for claiming and local pairing"
                        .into(),
                );
            }
            let out = sync_after(
                mesh,
                daemon_request(client, Request::NetworkUpdate { config }).await,
            )
            .await;
            // If the owner just changed the fleet mesh's transport (its venue),
            // push it to every member — the venue is owner-defined and
            // owner-broadcast (a no-op for a non-owner). The internal label
            // update in ensure_fleet_network goes straight to the daemon, not
            // through here, so this fires only on a deliberate config edit.
            if let (DispatchOut::Json(_), Some(nid)) = (&out, target) {
                if mesh.is_fleet_network(&nid) {
                    mesh.fleet_broadcast_config().await;
                }
            }
            out
        }
        "mesh_network_remove" => {
            let network: String = try_arg!(arg(a, "network"));
            // The local claim network can't be left, only switched on and
            // off (`network_set_enabled`) — a remove would be undone by the
            // next ownership check anyway, since the node re-joins it
            // whenever it isn't deliberately parked.
            if network == LOCAL_CLAIM_NETWORK_ID {
                return DispatchOut::Err(
                    "the Local network can't be left — switch it off instead".into(),
                );
            }
            sync_after(
                mesh,
                daemon_request(
                    client,
                    Request::NetworkRemove {
                        network,
                        purge: false,
                    },
                )
                .await,
            )
            .await
        }
        // The non-destructive twin of remove+re-add: redial signaling and
        // renegotiate ICE in place. Deliberately *not* wrapped in `sync_after`
        // — the network stays joined, so there's no prune / re-subscribe to do
        // (and no peer caches to drop, which is exactly what made the old
        // remove+re-add refresh strand the other side). Both args optional:
        // `network` reconnects every peer on that mesh (the global refresh);
        // `peer` alone reconnects one node on the mesh it's reachable on (the
        // per-node refresh); neither reconnects every joined mesh.
        "mesh_network_reconnect" => {
            let network: Option<String> = try_arg!(opt(a, "network"));
            let peer: Option<String> = try_arg!(opt(a, "peer"));
            json_result(mesh.reconnect(network, peer).await)
        }
        "mesh_identity_set_label" => {
            let label: String = try_arg!(arg(a, "label"));
            let out = daemon_request(
                client,
                Request::IdentitySetLabel {
                    label: label.clone(),
                },
            )
            .await;
            if let DispatchOut::Json(_) = &out {
                mesh.set_label(label).await;
            }
            out
        }

        // ---- closed-network governance + custody MFA (daemon passthroughs) ----
        "mesh_governance_state" => {
            let network: String = try_arg!(arg(a, "network"));
            daemon_request(client, Request::GovernanceState { network }).await
        }
        "mesh_governance_propose_kind" => {
            let network: String = try_arg!(arg(a, "network"));
            let to: String = try_arg!(arg(a, "to"));
            let mfa_code: Option<String> = try_arg!(opt(a, "mfa_code"));
            daemon_request(
                client,
                Request::GovernanceProposeKindChange {
                    network,
                    to,
                    mfa_code,
                },
            )
            .await
        }
        "mesh_governance_grant_role" => {
            let network: String = try_arg!(arg(a, "network"));
            let target: String = try_arg!(arg(a, "target"));
            let role: String = try_arg!(arg(a, "role"));
            let mfa_code: Option<String> = try_arg!(opt(a, "mfa_code"));
            daemon_request(
                client,
                Request::GovernanceProposeRoleGrant {
                    network,
                    target,
                    role,
                    mfa_code,
                },
            )
            .await
        }
        "mesh_governance_revoke_role" => {
            let network: String = try_arg!(arg(a, "network"));
            let target: String = try_arg!(arg(a, "target"));
            let mfa_code: Option<String> = try_arg!(opt(a, "mfa_code"));
            daemon_request(
                client,
                Request::GovernanceProposeRoleRevoke {
                    network,
                    target,
                    mfa_code,
                },
            )
            .await
        }
        "mesh_governance_sign" => {
            let network: String = try_arg!(arg(a, "network"));
            let proposal_id: String = try_arg!(arg(a, "proposal_id"));
            let mfa_code: Option<String> = try_arg!(opt(a, "mfa_code"));
            daemon_request(
                client,
                Request::GovernanceSign {
                    network,
                    proposal_id,
                    mfa_code,
                },
            )
            .await
        }
        "mesh_governance_deny" => {
            let network: String = try_arg!(arg(a, "network"));
            let proposal_id: String = try_arg!(arg(a, "proposal_id"));
            daemon_request(
                client,
                Request::GovernanceDeny {
                    network,
                    proposal_id,
                },
            )
            .await
        }
        "mesh_governance_withdraw" => {
            let network: String = try_arg!(arg(a, "network"));
            let proposal_id: String = try_arg!(arg(a, "proposal_id"));
            daemon_request(
                client,
                Request::GovernanceWithdraw {
                    network,
                    proposal_id,
                },
            )
            .await
        }
        "mesh_governance_spawn_split" => {
            let network: String = try_arg!(arg(a, "network"));
            let proposal_id: String = try_arg!(arg(a, "proposal_id"));
            daemon_request(
                client,
                Request::GovernanceSpawnSplit {
                    network,
                    proposal_id,
                },
            )
            .await
        }
        "mesh_governance_mfa_enroll" => {
            let network: String = try_arg!(arg(a, "network"));
            daemon_request(client, Request::GovernanceMfaEnroll { network }).await
        }
        "mesh_governance_mfa_status" => {
            let network: String = try_arg!(arg(a, "network"));
            daemon_request(client, Request::GovernanceMfaStatus { network }).await
        }
        "mesh_governance_mfa_disable" => {
            let network: String = try_arg!(arg(a, "network"));
            let code: String = try_arg!(arg(a, "code"));
            daemon_request(client, Request::GovernanceMfaDisable { network, code }).await
        }

        // ---- fleet custody MFA (targets the fleet's closed network) -------
        "fleet_mfa_status" => match mesh.fleet_network_id() {
            Some(network) => daemon_request(client, Request::GovernanceMfaStatus { network }).await,
            None => DispatchOut::Json(json!({ "enrolled": false, "no_fleet": true })),
        },
        "fleet_mfa_enroll" => match mesh.fleet_network_id() {
            Some(network) => daemon_request(client, Request::GovernanceMfaEnroll { network }).await,
            None => DispatchOut::Err(
                "not in a fleet yet — adopt a device to found one before enrolling".into(),
            ),
        },
        "fleet_mfa_disable" => {
            let code: String = try_arg!(arg(a, "code"));
            match mesh.fleet_network_id() {
                Some(network) => {
                    daemon_request(client, Request::GovernanceMfaDisable { network, code }).await
                }
                None => DispatchOut::Err("not in a fleet".into()),
            }
        }

        // ---- CEC Support -------------------------------------------------
        // The verbatim node-control surface the CEC Support client app and this
        // app's CEC tab both depend on. Technician commands (`cec_dial_node`,
        // the raised-hand answer; `cec_dial`, the number fallback) and customer
        // commands (`cec_online`, the approve/deny/revoke flow) share the one
        // dispatch; the events (`cec://request|peer|session|grants`) ride the
        // UiSink like every other engine event. Everything lives on the one
        // support area — the per-number room ops are gone.
        "cec_status" => json_result(mesh.cec_status().await),
        "cec_online" => json_result(mesh.cec_online().await),
        "cec_dial" => {
            let number: String = try_arg!(arg(a, "number"));
            let agent_name: String = try_arg!(opt(a, "agent_name")).unwrap_or_default();
            json_result(mesh.cec_dial(number, agent_name).await)
        }
        "cec_dial_node" => {
            let node: String = try_arg!(arg(a, "node"));
            let agent_name: String = try_arg!(opt(a, "agent_name")).unwrap_or_default();
            json_result(mesh.cec_dial_node(node, agent_name).await)
        }
        // The customer app's "name this computer" — an alias for the identity
        // label op (the machine label the help beacon and the technician's
        // card both read). Kept as a distinct name so the CEC Support client's
        // existing call resolves; without this arm it was a silent no-op.
        "cec_set_label" => {
            let label: String = try_arg!(arg(a, "label"));
            let out = daemon_request(
                client,
                Request::IdentitySetLabel {
                    label: label.clone(),
                },
            )
            .await;
            if let DispatchOut::Json(_) = &out {
                mesh.set_label(label).await;
            }
            out
        }
        "cec_pending" => json_result(mesh.cec_pending().await),
        "cec_approve" => {
            let tech: String = try_arg!(arg(a, "tech"));
            let scope: String = try_arg!(arg(a, "scope"));
            let session_id: String = try_arg!(arg(a, "session_id"));
            let want_control: bool = try_arg!(opt(a, "want_control")).unwrap_or(true);
            json_result(
                mesh.cec_approve(tech, scope, session_id, want_control)
                    .await,
            )
        }
        "cec_deny" => {
            let tech: String = try_arg!(arg(a, "tech"));
            let session_id: String = try_arg!(arg(a, "session_id"));
            json_result(mesh.cec_deny(tech, session_id).await)
        }
        "cec_chat_send" => {
            let peer: String = try_arg!(arg(a, "peer"));
            let text: String = try_arg!(arg(a, "text"));
            json_result(mesh.cec_chat_send(peer, text).await)
        }
        "cec_chat_history" => {
            let peer: String = try_arg!(arg(a, "peer"));
            json_result(mesh.cec_chat_history(peer).await)
        }
        "cec_revoke" => {
            let tech: String = try_arg!(arg(a, "tech"));
            json_result(mesh.cec_revoke(tech).await)
        }
        "cec_grants" => json_result(mesh.cec_grants().await),
        "cec_viewing" => json_result(mesh.cec_viewing().await),
        "cec_dialed" => json_result(mesh.cec_dialed().await),
        "cec_cancel_dial" => json_result(mesh.cec_cancel_dial().await),
        "cec_ask_help" => {
            let on: bool = try_arg!(opt(a, "on")).unwrap_or(true);
            json_result(mesh.cec_ask_help(on).await)
        }
        "cec_help_list" => json_result(mesh.cec_help_list().await),
        "cec_help_watch" => {
            let on: bool = try_arg!(opt(a, "on")).unwrap_or(true);
            json_result(mesh.cec_help_watch(on).await)
        }
        // "Forget this node" is an app-wide feature on every node's gear (drops
        // any node from the graph/roster + tears its session down). It lives on
        // the general `forget_node` op; `cec_forget_node` is kept as an alias so
        // the CEC client app's existing calls still resolve. The op itself layers
        // CEC cleanup on only when the peer is actually a CEC customer/technician.
        "forget_node" | "cec_forget_node" => {
            let node: String = try_arg!(arg(a, "node"));
            json_result(mesh.forget_node(node).await)
        }

        // ---- park store --------------------------------------------------
        "disabled_networks" => DispatchOut::Json(Value::Array(disabled.list())),
        "network_set_enabled" => {
            let network: String = try_arg!(arg(a, "network"));
            let enabled: bool = try_arg!(arg(a, "enabled"));
            network_set_enabled(mesh, client, disabled, network, enabled).await
        }

        other => DispatchOut::Err(format!("unknown node command: {other}")),
    }
}

/// One daemon round trip, unwrapped into a `DispatchOut`: `!ok` → `Err`,
/// else the response data (or `null`) as JSON. Mirrors the GUI's
/// `unwrap_response`.
async fn daemon_request(client: &Arc<ControlClient>, req: Request) -> DispatchOut {
    match client.request(&req).await {
        Ok(resp) if resp.ok => DispatchOut::Json(resp.data.unwrap_or(Value::Null)),
        Ok(resp) => DispatchOut::Err(resp.error.unwrap_or_else(|| "(no error message)".into())),
        Err(e) => DispatchOut::Err(e.to_string()),
    }
}

/// After a successful network add/update/remove, re-subscribe + re-advertise
/// so the change lights up this session immediately (mirrors the GUI's
/// `sync_networks` call after each). On error, pass the error through
/// untouched.
async fn sync_after(mesh: &Arc<Mesh>, out: DispatchOut) -> DispatchOut {
    if let DispatchOut::Json(_) = &out {
        mesh.sync_networks().await;
    }
    out
}

/// Switch a network off or back on without deleting it — the exact body of the
/// GUI's `network_set_enabled` command (gui/src-tauri/src/main.rs). Enable:
/// take the parked config, hand it back to the daemon, sync (re-park on
/// failure). Disable: snapshot the full config from `config_show`, park it,
/// leave the daemon, sync (un-park on failure).
async fn network_set_enabled(
    mesh: &Arc<Mesh>,
    client: &Arc<ControlClient>,
    disabled: &Arc<DisabledNetworks>,
    network: String,
    enabled: bool,
) -> DispatchOut {
    if enabled {
        let Some(config) = disabled.take(&network) else {
            return DispatchOut::Err(format!("'{network}' isn't a disabled network here"));
        };
        let rejoin = daemon_request(
            client,
            Request::NetworkAdd {
                config: config.clone(),
            },
        )
        .await;
        match rejoin {
            DispatchOut::Json(data) => {
                mesh.sync_networks().await;
                DispatchOut::Json(data)
            }
            other => {
                // Park it back so a failed re-join never loses the config.
                disabled.park(config);
                other
            }
        }
    } else {
        // Snapshot the full config *before* leaving — `config_show` is the
        // only place the daemon hands the whole thing back.
        let shown = match daemon_request(client, Request::ConfigShow).await {
            DispatchOut::Json(v) => v,
            other => return other,
        };
        let config = shown
            .pointer("/config/networks")
            .and_then(|v| v.as_array())
            .and_then(|nets| {
                nets.iter()
                    .find(|n| {
                        let id = n.get("id").and_then(|v| v.as_str()).unwrap_or_default();
                        let nid = n
                            .get("network_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default();
                        id == network || nid == network
                    })
                    .cloned()
            });
        let Some(config) = config else {
            return DispatchOut::Err(format!("unknown network: {network}"));
        };
        if !disabled.park(config) {
            return DispatchOut::Err(
                "couldn't save the network for later — not disabling it".into(),
            );
        }
        let left = daemon_request(
            client,
            Request::NetworkRemove {
                network: network.clone(),
                // Disabling parks the config for re-enable — keep its state.
                purge: false,
            },
        )
        .await;
        match left {
            DispatchOut::Json(data) => {
                mesh.sync_networks().await;
                DispatchOut::Json(data)
            }
            other => {
                // Still joined — un-park so the books match reality.
                let _ = disabled.take(&network);
                other
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ensure_node_running — spawn + probe an `allmystuff-serve` node
// ---------------------------------------------------------------------------

/// Owned wrapper around a spawned `allmystuff-serve` node child. Dropping it
/// kills the child (mirrors [`crate::daemon_spawn::DaemonChild`]).
pub struct NodeChild {
    child: Option<Child>,
}

impl NodeChild {
    /// Whether the spawned node process is still running (`false` for one that
    /// exited, or a handle that never held a child). Lets a client tell "the
    /// control socket is busy" apart from "the serve is gone": respawning over
    /// a live serve spawns a bind-loser and then kills the live serve when the
    /// old handle is dropped — the spawn/kill metronome. Check this first.
    pub fn is_alive(&mut self) -> bool {
        match self.child.as_mut() {
            Some(c) => matches!(c.try_wait(), Ok(None)),
            None => false,
        }
    }
}

impl Drop for NodeChild {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            // SIGTERM-then-SIGKILL on unix so a clean parent exit lets the node
            // run its own shutdown — which drops its `DaemonChild` and cascades
            // the kill to the mesh daemon, instead of orphaning it. Windows
            // keeps the job-object kill-on-close (see `graceful_kill`).
            crate::daemon_spawn::graceful_kill(&mut c);
            let _ = c.wait();
            tracing::info!("allmystuff node child terminated");
        }
    }
}

/// Tie the spawned node's lifetime to this process at the OS level, so a crash
/// or force-kill of the parent doesn't orphan the node (which would keep this
/// machine's identity live and swallow its traffic). Linux uses
/// `PR_SET_PDEATHSIG` (set in `pre_exec` at spawn); Windows a kill-on-close
/// job object; macOS relies on the `Drop` kill for clean exits. Mirrors
/// [`crate::daemon_spawn`]'s tie.
#[cfg(windows)]
fn tie_node_lifetime(child: &Child) {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            tracing::warn!("couldn't create a job object for the node — a crash may orphan it");
            return;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) != 0
            && AssignProcessToJobObject(job, child.as_raw_handle() as _) != 0;
        if ok {
            tracing::info!("node tied to this process (job object, kill-on-close)");
            // Leak the job handle so the kernel's close (on our exit) kills it.
        } else {
            tracing::warn!("couldn't tie the node to this process — a crash may orphan it");
            CloseHandle(job);
        }
    }
}

#[cfg(not(windows))]
fn tie_node_lifetime(_child: &Child) {
    // Linux is handled in `pre_exec` (PR_SET_PDEATHSIG); macOS has no
    // kernel-level equivalent.
}

/// A real, non-empty binary — not the zero-byte sidecar stub the GUI's
/// `build.rs` writes when `allmystuff-serve` wasn't built (spawning that stub is
/// exactly the "node never came up" failure).
fn usable(p: &std::path::Path) -> bool {
    p.metadata()
        .map(|m| m.is_file() && m.len() > 0)
        .unwrap_or(false)
}

/// Locate the `allmystuff-serve` node binary. It ships the same way the
/// `myownmesh` daemon does — a Tauri sidecar beside the app — so this mirrors
/// [`crate::daemon_spawn::find_daemon_binary`], with one twist: unlike the
/// fetched daemon, the node is built *locally*, so a dev run's freshest copy is
/// in the sibling `node/target`, which we prefer over a possibly-stale sidecar:
///
///  1. `ALLMYSTUFF_SERVE_BIN` override.
///  2. **Sibling node build** — `node/target/{release,debug}/allmystuff-serve`
///     (what `just dev` builds; the build-time manifest dir is `<repo>/node`,
///     so this path only exists on a dev machine).
///  3. **Bundled sidecar** beside the running exe — `allmystuff-serve-<triple>`
///     (dev staging) or plain `allmystuff-serve{.exe}` (production bundle).
///  4. **Dev source slot** — `gui/src-tauri/binaries/allmystuff-serve-<triple>`.
///  5. `allmystuff-serve` on `$PATH` (an installed copy).
///
/// Every candidate is checked with [`usable`] so a stub is skipped.
fn find_node_binary() -> Option<(PathBuf, NodeSource)> {
    let exe = format!("allmystuff-serve{}", std::env::consts::EXE_SUFFIX);
    let exe_triple = format!(
        "allmystuff-serve-{}{}",
        env!("DAEMON_SIDECAR_TRIPLE"),
        std::env::consts::EXE_SUFFIX
    );

    // 1. Override.
    if let Some(p) = std::env::var_os("ALLMYSTUFF_SERVE_BIN") {
        let p = PathBuf::from(p);
        if usable(&p) {
            return Some((p, NodeSource::Override));
        }
    }

    // 2. Sibling node build (freshest in dev; `CARGO_MANIFEST_DIR` is `<repo>/node`).
    let node_target = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target");
    for profile in ["release", "debug"] {
        let p = node_target.join(profile).join(&exe);
        if usable(&p) {
            return Some((p, NodeSource::DevBuild));
        }
    }

    // 3. Bundled sidecar beside the running app binary. The triple-suffixed
    //    name is Tauri's dev staging (a dev artifact); the plain name is the
    //    production bundle an installed app ships — the one kind we keep current.
    if let Ok(cur) = std::env::current_exe() {
        if let Some(dir) = cur.parent() {
            for (name, source) in [
                (exe_triple.as_str(), NodeSource::DevBuild),
                (exe.as_str(), NodeSource::Installed),
            ] {
                let p = dir.join(name);
                if usable(&p) {
                    return Some((p, source));
                }
            }
        }
    }

    // 4. Dev source slot the GUI's build.rs stages into.
    if let Some(root) = PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent() {
        let p = root
            .join("gui")
            .join("src-tauri")
            .join("binaries")
            .join(&exe_triple);
        if usable(&p) {
            return Some((p, NodeSource::DevBuild));
        }
    }

    // 5. PATH (an installed copy).
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(&exe);
            if usable(&candidate) {
                return Some((candidate, NodeSource::Installed));
            }
        }
    }
    None
}

/// Where [`find_node_binary`] found the `allmystuff-serve` node — decides
/// whether it's ours to keep current against a caller's pin. Mirrors
/// [`crate::daemon_spawn::DaemonSource`] for the node binary.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NodeSource {
    /// Explicit `ALLMYSTUFF_SERVE_BIN` override — deliberately pinned, never
    /// touched.
    Override,
    /// An installed copy: the production bundle's sidecar beside the app, or an
    /// `allmystuff-serve` on `$PATH`. The only kind we ask to self-update.
    Installed,
    /// A dev artifact (the sibling `node/target` build, the dev-staged sidecar,
    /// the `build.rs` source slot) — never touched; self-updating one would
    /// clobber a local build with a release download.
    DevBuild,
}

/// `<bin> --version`, parsed to `(major, minor, patch)`. `None` when the binary
/// won't answer or prints an unparseable line.
async fn node_binary_version(bin: &Path) -> Option<(u64, u64, u64)> {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let out = tokio::time::timeout(Duration::from_secs(10), cmd.output())
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    crate::daemon_spawn::parse_version_output(&String::from_utf8_lossy(&out.stdout))
}

/// `allmystuff-serve update` downloads a release binary — give it real time, but
/// never wedge bring-up forever on a stalled network. Mirrors
/// [`crate::daemon_spawn`]'s daemon-update budget.
const NODE_UPDATE_TIMEOUT: Duration = Duration::from_secs(180);

/// Run `<bin> update` (the node's own self-updater) and report whether the
/// binary on disk now satisfies `want`. Output is folded into our log; failure
/// never propagates (an old node still beats no node).
async fn run_node_update(bin: &Path, want: (u64, u64, u64)) -> bool {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.arg("update")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    match tokio::time::timeout(NODE_UPDATE_TIMEOUT, cmd.output()).await {
        Err(_) => tracing::warn!(
            "allmystuff-serve update didn't finish within {}s — continuing with what's on disk",
            NODE_UPDATE_TIMEOUT.as_secs()
        ),
        Ok(Err(e)) => tracing::warn!("couldn't run allmystuff-serve update: {e}"),
        Ok(Ok(out)) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let said = stdout.trim();
            if !said.is_empty() {
                tracing::info!("allmystuff-serve update: {said}");
            }
            if !out.status.success() {
                let stderr = String::from_utf8_lossy(&out.stderr);
                tracing::warn!("allmystuff-serve update failed: {}", stderr.trim());
            }
        }
    }
    // The re-check decides — `update` may have been refused (package-managed),
    // failed, or landed exactly the pin.
    match node_binary_version(bin).await {
        Some(have) => have >= want,
        None => false,
    }
}

/// When a caller pins a specific AllMyStuff version, make sure the node binary
/// we're about to start meets it: an **Installed** `allmystuff-serve` reporting
/// a version below the pin is asked to update itself (`allmystuff-serve update`)
/// first. A dev/override binary is left alone, and a non-comparable pin (a
/// sha/branch, not `vX.Y.Z`) is a no-op. This is the node-side twin of
/// [`crate::daemon_spawn`]'s `ensure_daemon_current`.
async fn ensure_node_current(bin: &Path, pin: &str) {
    let Some(want) = crate::daemon_spawn::parse_semverish(pin) else {
        return;
    };
    match node_binary_version(bin).await {
        None => tracing::warn!(
            "couldn't read {}'s version to compare against the {pin} pin",
            bin.display()
        ),
        Some(have) if have >= want => {}
        Some(have) => {
            tracing::info!(
                "allmystuff-serve at {} is v{} but this app pins {pin} — asking it to update itself (allmystuff-serve update)…",
                bin.display(),
                crate::daemon_spawn::fmt_ver(have)
            );
            if run_node_update(bin, want).await {
                tracing::info!("allmystuff-serve is current — starting the updated node");
            } else {
                tracing::warn!(
                    "couldn't bring allmystuff-serve up to {pin}; starting what's on disk — some pinned features may be unavailable. Update it by hand: allmystuff-serve update"
                );
            }
        }
    }
}

/// Make sure a node is running, spawning one if not. Returns `Ok(None)` when a
/// node already answers the control socket (we reuse it), `Ok(Some(child))`
/// when we started one (the handle kills it on drop). Mirrors
/// [`crate::daemon_spawn::ensure_daemon_running`]'s shape; the GUI will call
/// this in Phase B.
pub async fn ensure_node_running() -> Result<Option<NodeChild>> {
    // Pin to OUR OWN version — this crate is compiled into whatever app is
    // calling, so its version is the version that app was built against. A
    // reused node older than that is a skew by definition, and since the Always
    // On service is the default backend it is the skew an app self-update
    // leaves behind every time. Convergence is therefore the default rather
    // than something only the CEC app opts into; pass an explicit pin to
    // ensure_node_running_pinned to override it.
    ensure_node_running_impl(Some(env!("CARGO_PKG_VERSION")), false).await
}

/// Like [`ensure_node_running`], but the caller supplies the AllMyStuff version
/// it was built against (its *pin*). A reused or about-to-be-spawned
/// **Installed** `allmystuff-serve` older than that pin is asked to update
/// itself first — the same "keep a sidecar you don't own current" move
/// AllMyStuff makes for a reused `myownmesh` (see [`crate::daemon_spawn`]). The
/// CEC Support app uses this so a separately-installed AllMyStuff node it reuses
/// is brought up to the version CEC needs to work properly. On Windows this
/// CEC-specific entry point also rejects a Session 0 media/input node. A
/// `pin = None` skips only the version check.
pub async fn ensure_node_running_pinned(pin: Option<&str>) -> Result<Option<NodeChild>> {
    ensure_node_running_impl(pin, true).await
}

#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PinnedNodeAction {
    Reuse,
    Spawn,
}

/// Decide whether CEC Support may reuse or create the shared node.
///
/// CEC's Windows service runs as LocalSystem in Session 0. It may use an
/// already-running interactive node, but it must never create or reuse a media
/// and input host in Session 0.
#[cfg(any(windows, test))]
fn pinned_node_action(caller_session: u32, node_session: Option<u32>) -> Result<PinnedNodeAction> {
    match node_session {
        Some(0) => bail!(
            "the shared AllMyStuff node is running in Windows Session 0; \
             refusing to attach desktop capture or input"
        ),
        Some(_) => Ok(PinnedNodeAction::Reuse),
        None if caller_session == 0 => bail!(
            "no interactive AllMyStuff node is running; refusing to launch \
             desktop capture or input from Windows Session 0"
        ),
        None => Ok(PinnedNodeAction::Spawn),
    }
}

#[cfg(windows)]
fn windows_process_session(pid: u32) -> Result<u32> {
    let mut session_id = 0u32;
    let ok = unsafe {
        windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId(pid, &mut session_id)
    };
    if ok == 0 {
        bail!(
            "resolve Windows SessionId for PID {pid}: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(session_id)
}

#[cfg(windows)]
async fn windows_node_process_session() -> Result<(u32, u32)> {
    let client = NodeClient::new()?;
    let stream = client
        .connect()
        .await
        .context("connect to inspect the shared node owner")?;
    let pid = stream
        .peer_creds()
        .context("read shared node pipe credentials")?
        .pid()
        .ok_or_else(|| anyhow!("the shared node pipe did not report an owner PID"))?;
    let session_id = windows_process_session(pid)?;
    Ok((pid, session_id))
}

/// What to do about a reused node that may be behind the caller's pin.
///
/// Split out as a pure decision so the ordering that matters can be tested
/// without a socket, a service manager, or a network.
#[derive(Debug, PartialEq, Eq)]
enum Converge {
    /// Running node meets the pin (or we can't tell) — leave it be.
    Nothing,
    /// Disk is already current; only the running process is stale. This is the
    /// state a just-updated app leaves behind, because applying an update
    /// rewrites allmystuff-serve on disk and cannot touch a running service.
    RestartOnly,
    /// Disk is behind too: update it first, then restart onto it.
    UpdateThenRestart,
}

/// `running`/`disk` are `None` when that half wouldn't report a version.
///
/// The restart is deliberately gated on the DISK binary meeting the pin. A
/// restart that lands on the same old build changes nothing and costs the user
/// a bounced service — and because a node restart makes an attached GUI
/// relaunch too, an ungated version would restart both halves on every single
/// launch for as long as the update kept failing. Only ever ask when the
/// restart has somewhere newer to land.
fn converge_action(
    running: Option<(u64, u64, u64)>,
    disk: Option<(u64, u64, u64)>,
    want: (u64, u64, u64),
) -> Converge {
    // No answer from the running node means an older build with no
    // `node_version` op — or a node mid-restart. Either way, guessing it is
    // stale and bouncing it is worse than waiting for its own updater.
    let Some(running) = running else {
        return Converge::Nothing;
    };
    if running >= want {
        return Converge::Nothing;
    }
    match disk {
        Some(disk) if disk >= want => Converge::RestartOnly,
        // Includes disk == None: an unreadable binary still gets the update
        // attempt, and run_node_update's re-check decides whether the restart
        // actually happens.
        _ => Converge::UpdateThenRestart,
    }
}

/// Bring a node we did not spawn up to `pin` — binary *and* process.
///
/// The half that was missing: applying a self-update rewrites every installed
/// half on disk, including `allmystuff-serve`, but it cannot restart a service
/// it doesn't own. The old code updated the binary and told the user to "quit
/// whatever started it (or reboot)", which is not a thing a customer does —
/// and with the Always On service now the default backend, that skew is the
/// normal state after every update, not an edge case. The node's own
/// unattended updater would close it eventually, but only on its 24-hour tick.
///
/// The primary path asks the running node to update the installation it owns,
/// then verifies the process that returns. [`converge_action`] remains only for
/// the 0.2.65 compatibility path, before `request_update` existed.
async fn running_node_version(client: &NodeClient) -> Option<(u64, u64, u64)> {
    client
        .request("node_version", serde_json::json!({}))
        .await
        .ok()
        .and_then(|value| {
            value
                .get("version")
                .and_then(|version| version.as_str())
                .and_then(crate::daemon_spawn::parse_semverish)
        })
}

/// Wait through the update's socket drop/relaunch and prove the process that
/// comes back actually satisfies the caller's pin. A successful request is not
/// convergence; the version reported by the replacement process is.
async fn wait_for_running_node_version(
    client: &NodeClient,
    want: (u64, u64, u64),
    timeout: Duration,
) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if running_node_version(client)
            .await
            .is_some_and(|running| running >= want)
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    false
}

/// Reconcile the process answering the shared node socket with a consumer's
/// minimum AllMyStuff version. Public so the desktop shell can decide whether a
/// legacy Windows service needs the one-time elevated repair path.
pub async fn reconcile_running_node(pin: &str) -> bool {
    converge_reused_node(pin).await
}

/// Cheap post-start verification used after reconciliation or a legacy service
/// repair. Unlike [`reconcile_running_node`], this never downloads or restarts.
pub async fn running_node_satisfies(pin: &str) -> bool {
    let Some(want) = crate::daemon_spawn::parse_semverish(pin) else {
        return true;
    };
    let Some(client) = NodeClient::new().ok() else {
        return false;
    };
    running_node_version(&client)
        .await
        .is_some_and(|running| running >= want)
}

async fn converge_reused_node(pin: &str) -> bool {
    let Some(want) = crate::daemon_spawn::parse_semverish(pin) else {
        return true;
    };
    let Some(client) = NodeClient::new().ok() else {
        return false;
    };
    let running = running_node_version(&client).await;
    if running.is_some_and(|version| version >= want) {
        return true;
    }

    // Ask the owner of the socket to update the installation it is actually
    // executing. This is the path that correctly reaches a protected
    // ProgramData service copy and is shared by AllMyStuff and CEC Support.
    match client
        .request("request_update", serde_json::json!({ "minimum": pin }))
        .await
    {
        Ok(_) => {
            if wait_for_running_node_version(
                &client,
                want,
                NODE_UPDATE_TIMEOUT + Duration::from_secs(20),
            )
            .await
            {
                return true;
            }
            tracing::warn!(
                "the shared node accepted the {pin} update request but did not return at that version"
            );
            return false;
        }
        Err(error) => tracing::warn!(
            "the shared node could not accept a dependency update request ({error}); trying the pre-request compatibility path"
        ),
    }

    // Compatibility for 0.2.65-era nodes: update the ordinary installed
    // sidecar and ask the running process to restart. This works when that
    // sidecar is the process owner. A protected Windows service is a distinct
    // copy, so verification below intentionally fails and lets the desktop
    // shell perform its one-time service repair.
    // Only an Installed node is ours to update; a dev/override build is the
    // developer's business, exactly as before.
    let installed = match find_node_binary() {
        Some((bin, NodeSource::Installed)) => Some(bin),
        _ => None,
    };
    let disk = match &installed {
        Some(bin) => node_binary_version(bin).await,
        None => None,
    };
    match converge_action(running, disk, want) {
        Converge::Nothing => false,
        Converge::UpdateThenRestart => {
            let Some(bin) = installed else {
                tracing::warn!(
                    "the running allmystuff-serve is below the {pin} pin, but its binary isn't one we install — leaving it alone"
                );
                return false;
            };
            tracing::info!(
                "the reused allmystuff-serve is below the {pin} pin — updating it on disk…"
            );
            if !run_node_update(&bin, want).await {
                tracing::warn!(
                    "couldn't bring allmystuff-serve to {pin} on disk — NOT restarting it, since it would come back the same version"
                );
                return false;
            }
            request_node_restart(&client, pin).await
                && wait_for_running_node_version(&client, want, Duration::from_secs(20)).await
        }
        Converge::RestartOnly => {
            request_node_restart(&client, pin).await
                && wait_for_running_node_version(&client, want, Duration::from_secs(20)).await
        }
    }
}

/// Ask the running node to relaunch onto the on-disk build. Best-effort: the
/// node answers *before* it goes, so a transport error here means an older
/// node with no `restart_self` op — say so plainly rather than leaving a
/// silent skew.
async fn request_node_restart(client: &NodeClient, pin: &str) -> bool {
    tracing::info!(
        "allmystuff-serve on disk now meets {pin} but the running node is older — asking it to relaunch onto it"
    );
    match client.request("restart_self", serde_json::json!({})).await {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!(
                "the running node wouldn't take a restart ({e}) — it predates `restart_self`, so it keeps the old build until its own updater or a service restart bounces it"
            );
            false
        }
    }
}

async fn ensure_node_running_impl(
    pin: Option<&str>,
    require_interactive_windows_node: bool,
) -> Result<Option<NodeChild>> {
    #[cfg(windows)]
    let caller = if require_interactive_windows_node {
        let pid = std::process::id();
        let session_id = windows_process_session(pid)?;
        tracing::info!(
            caller_pid = pid,
            caller_session_id = session_id,
            "validating CEC Support node session ownership"
        );
        Some((pid, session_id))
    } else {
        None
    };
    #[cfg(not(windows))]
    let _ = require_interactive_windows_node;

    if NodeClient::probe().await {
        #[cfg(windows)]
        if let Some((caller_pid, caller_session_id)) = caller {
            let (node_pid, node_session_id) = windows_node_process_session().await?;
            pinned_node_action(caller_session_id, Some(node_session_id))?;
            tracing::info!(
                caller_pid,
                caller_session_id,
                node_pid,
                node_session_id,
                "CEC Support is reusing an interactive AllMyStuff node"
            );
        }
        tracing::info!("existing allmystuff node found on the control socket");
        // A node we didn't spawn is already serving. Bring it up to the pin —
        // both halves of it: the binary on disk, and the process actually
        // running. See converge_reused_node.
        if let Some(pin) = pin {
            converge_reused_node(pin).await;
        }
        return Ok(None);
    }

    #[cfg(windows)]
    if let Some((caller_pid, caller_session_id)) = caller {
        pinned_node_action(caller_session_id, None)?;
        tracing::info!(
            caller_pid,
            caller_session_id,
            "CEC Support may launch the node in this interactive session"
        );
    }

    let (bin, source) = find_node_binary().ok_or_else(|| {
        anyhow!(
            "couldn't find the `allmystuff-serve` node binary — it normally ships beside \
             this app; put it on PATH or run `allmystuff serve` yourself"
        )
    })?;
    // Keep an Installed node current against the caller's pin before starting
    // it — a below-pin node answers the socket fine but silently lacks the
    // features this app was built against.
    if let (Some(pin), NodeSource::Installed) = (pin, source) {
        ensure_node_current(&bin, pin).await;
    }
    tracing::info!(?bin, "spawning allmystuff node");

    let mut cmd = Command::new(&bin);
    cmd.stdin(Stdio::null());
    // Unix: the child inherits our stdout/stderr, so its logs stream into the
    // same terminal (`just dev` / `allmystuff serve`).
    #[cfg(not(windows))]
    {
        cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());
    }
    // Windows: a GUI-subsystem parent has no console for the child to inherit,
    // so inherited stdout would vanish — the node's logs never reach the
    // terminal (only its file). Capture them and forward to ours below so they
    // stream inline like on Unix. CREATE_NO_WINDOW still stops a console window
    // from flashing up.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    // Linux half of the lifetime tie: SIGKILL the node when this process dies.
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt as _;
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn {}", bin.display()))?;
    #[cfg(not(windows))]
    let child = cmd
        .spawn()
        .with_context(|| format!("spawn {}", bin.display()))?;
    // Windows: pump the node's captured stdout/stderr into ours, line by line,
    // so its logs show in the `just dev` terminal (detached — they end when the
    // node exits and the pipes close).
    #[cfg(windows)]
    {
        use std::io::{BufRead, BufReader, Write};
        if let Some(out) = child.stdout.take() {
            std::thread::spawn(move || {
                let mut sink = std::io::stdout();
                for line in BufReader::new(out).lines().map_while(Result::ok) {
                    let _ = writeln!(sink, "{line}");
                }
            });
        }
        if let Some(err) = child.stderr.take() {
            std::thread::spawn(move || {
                let mut sink = std::io::stderr();
                for line in BufReader::new(err).lines().map_while(Result::ok) {
                    let _ = writeln!(sink, "{line}");
                }
            });
        }
    }
    tie_node_lifetime(&child);
    let handle = NodeChild { child: Some(child) };

    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(150)).await;
        if NodeClient::probe().await {
            tracing::info!("allmystuff node up");
            return Ok(Some(handle));
        }
    }
    tracing::warn!("node did not answer within 8s; leaving it running — callers will retry");
    Ok(Some(handle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pinned_node_session_policy_allows_only_interactive_media_hosts() {
        assert_eq!(
            pinned_node_action(0, Some(1)).unwrap(),
            PinnedNodeAction::Reuse
        );
        assert_eq!(
            pinned_node_action(1, Some(1)).unwrap(),
            PinnedNodeAction::Reuse
        );
        assert_eq!(
            pinned_node_action(1, None).unwrap(),
            PinnedNodeAction::Spawn
        );
        assert!(pinned_node_action(0, None).is_err());
        assert!(pinned_node_action(1, Some(0)).is_err());
        assert!(pinned_node_action(0, Some(0)).is_err());
    }

    /// AMS-04: the Unix control socket must be bound owner-only (0600) so no
    /// other local user/process can reach the privileged control API. Binds a
    /// real socket and checks the mode `bind_owner_only` chmod'd it to.
    #[cfg(unix)]
    #[tokio::test]
    async fn control_socket_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        // Keep the path short — macOS caps Unix socket paths at ~104 bytes.
        let path = std::env::temp_dir().join(format!("ams-node-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let addr = SocketAddr::Path(path.clone());
        let listener = bind_owner_only(&addr).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "node control socket must be owner-only");
        drop(listener);
        let _ = std::fs::remove_file(&path);
    }

    /// Round-trip a frame through an in-memory duplex pipe and assert the tag
    /// and payload survive intact.
    async fn round_trip(tag: u8, payload: Vec<u8>) {
        let (mut a, mut b) = tokio::io::duplex(1024 * 1024);
        let p2 = payload.clone();
        let writer = tokio::spawn(async move {
            write_frame(&mut a, tag, &p2).await.unwrap();
        });
        let (got_tag, got_payload) = read_frame(&mut b).await.unwrap().expect("a frame");
        writer.await.unwrap();
        assert_eq!(got_tag, tag);
        assert_eq!(got_payload, payload);
    }

    #[tokio::test]
    async fn frame_round_trip_json() {
        let body = serde_json::to_vec(&json!({ "ok": true, "n": 7 })).unwrap();
        round_trip(TAG_JSON, body).await;
    }

    #[tokio::test]
    async fn frame_round_trip_empty_and_bytes() {
        round_trip(TAG_BYTES, Vec::new()).await;
        round_trip(TAG_BYTES, vec![0, 1, 2, 3, 255, 254]).await;
    }

    #[tokio::test]
    async fn frame_round_trip_100kb_blob() {
        let blob: Vec<u8> = (0..100_000u32).map(|i| (i % 256) as u8).collect();
        round_trip(TAG_BYTES, blob).await;
    }

    #[tokio::test]
    async fn read_frame_clean_eof_is_none() {
        let (a, mut b) = tokio::io::duplex(64);
        drop(a); // EOF before any byte of a frame
        assert!(read_frame(&mut b).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn read_frame_rejects_oversized_length() {
        let (mut a, mut b) = tokio::io::duplex(64);
        // A length past the ceiling must error before any allocation.
        let bogus = (MAX_FRAME_LEN as u32 + 1).to_be_bytes();
        a.write_all(&bogus).await.unwrap();
        a.flush().await.unwrap();
        drop(a);
        assert!(read_frame(&mut b).await.is_err());
    }

    #[test]
    fn node_request_serde_round_trip() {
        let req = NodeRequest {
            cmd: "connect_route".into(),
            args: json!({ "from": "a", "to": "b", "media": "video" }),
        };
        let bytes = serde_json::to_vec(&req).unwrap();
        let back: NodeRequest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.cmd, "connect_route");
        assert_eq!(back.args["media"], "video");
        // A request with no args field defaults to Null.
        let bare: NodeRequest = serde_json::from_str(r#"{"cmd":"scan_self"}"#).unwrap();
        assert_eq!(bare.cmd, "scan_self");
        assert_eq!(bare.args, Value::Null);
    }

    #[test]
    fn node_event_serde_round_trip() {
        let emit = NodeEvent::Emit {
            event: "allmystuff://session".into(),
            payload: json!({ "peers": [] }),
        };
        let bytes = serde_json::to_vec(&emit).unwrap();
        match serde_json::from_slice::<NodeEvent>(&bytes).unwrap() {
            NodeEvent::Emit { event, payload } => {
                assert_eq!(event, "allmystuff://session");
                assert_eq!(payload["peers"], json!([]));
            }
            _ => panic!("expected Emit"),
        }
        let restart = serde_json::to_vec(&NodeEvent::Restart).unwrap();
        assert!(matches!(
            serde_json::from_slice::<NodeEvent>(&restart).unwrap(),
            NodeEvent::Restart
        ));
    }

    /// The skew this whole path exists for: an app self-update rewrote
    /// allmystuff-serve on disk, but the Always On service — now the default
    /// backend — is still executing the image it started with. Nothing on disk
    /// is wrong, so the old on-disk-only check saw a current binary and did
    /// nothing at all, while every fix in that update silently wasn't there.
    #[test]
    fn a_stale_service_on_a_current_binary_is_restarted() {
        assert_eq!(
            converge_action(Some((0, 2, 63)), Some((0, 2, 64)), (0, 2, 64)),
            Converge::RestartOnly
        );
    }

    /// Both halves behind (the node's own updater never ran): update the disk
    /// first, then restart onto it. Restarting first would land on the same
    /// build.
    #[test]
    fn a_stale_binary_is_updated_before_the_restart() {
        assert_eq!(
            converge_action(Some((0, 2, 60)), Some((0, 2, 60)), (0, 2, 64)),
            Converge::UpdateThenRestart
        );
        // An unreadable binary still gets the attempt; run_node_update's
        // re-check is what decides whether the restart follows.
        assert_eq!(
            converge_action(Some((0, 2, 60)), None, (0, 2, 64)),
            Converge::UpdateThenRestart
        );
    }

    /// A node at or past the pin is left alone — including one AHEAD of the
    /// app, which is an app that hasn't relaunched yet, not a node to bounce.
    #[test]
    fn a_current_node_is_left_alone() {
        assert_eq!(
            converge_action(Some((0, 2, 64)), Some((0, 2, 64)), (0, 2, 64)),
            Converge::Nothing
        );
        assert_eq!(
            converge_action(Some((0, 2, 65)), Some((0, 2, 65)), (0, 2, 64)),
            Converge::Nothing
        );
    }

    /// A node that won't say what it is predates `node_version`, so it also
    /// predates `restart_self` — and may simply be mid-restart. Bouncing it on
    /// a guess is worse than waiting for its own updater.
    #[test]
    fn a_silent_node_is_never_bounced_on_a_guess() {
        assert_eq!(
            converge_action(None, Some((0, 2, 60)), (0, 2, 64)),
            Converge::Nothing
        );
        assert_eq!(converge_action(None, None, (0, 2, 64)), Converge::Nothing);
    }

    /// The loop guard. A node restart makes an attached GUI relaunch too, so
    /// asking for one that can't change the version would restart BOTH halves
    /// on every launch, forever. The restart is gated on disk >= pin precisely
    /// so it always has somewhere newer to land.
    #[test]
    fn a_restart_is_never_asked_for_when_it_would_change_nothing() {
        for disk in [Some((0, 2, 60)), Some((0, 2, 63)), None] {
            assert_ne!(
                converge_action(Some((0, 2, 63)), disk, (0, 2, 64)),
                Converge::RestartOnly,
                "disk {disk:?} cannot satisfy the pin, so a bare restart would spin"
            );
        }
    }
}
