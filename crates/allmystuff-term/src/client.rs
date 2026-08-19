//! Client of a running AllMyStuff node's control socket.
//!
//! This is a small, self-contained **mirror** of the node-side wire protocol
//! defined in `allmystuff-node`'s `node_control` module — the same way
//! [`allmystuff_protocol`] is "a dependency-free mirror of the MyOwnMesh daemon
//! control socket". `amst` is a thin terminal client: it has no business
//! linking the heavy node engine (xcap / openh264 / cpal / …), so it re-states
//! just the handful of frame shapes and the socket address it needs and speaks
//! the wire directly.
//!
//! Wire format (must stay byte-identical to the node): length-prefixed frames
//! `[u32 BE len][1 tag byte][payload]`, where `len` counts the tag plus the
//! payload. The node serves a connection's request lines in order, so a
//! one-shot command is connect → write one frame → read one frame → close, and
//! the event stream is connect → write the subscribe sentinel → read the ack →
//! read frames forever.

use std::time::Duration;

use interprocess::local_socket::tokio::prelude::*;
#[cfg(unix)]
use interprocess::local_socket::GenericFilePath;
#[cfg(not(unix))]
use interprocess::local_socket::GenericNamespaced;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

// ---- frame tags (mirror node_control) --------------------------------------

/// A JSON-bodied frame: a request in, or a command's `{ok,result,error}`
/// response (and the event-stream ack) out.
const TAG_JSON: u8 = 0;
/// A raw-bytes frame: the response to a poll command (`term_poll`), whose body
/// is the node's already length-framed batch — kept binary, not re-encoded.
const TAG_BYTES: u8 = 1;
/// One streamed engine event on the long-lived event connection.
const TAG_EVENT: u8 = 2;
/// The node re-execing onto a staged update, streamed just before it restarts.
const TAG_RESTART: u8 = 3;

/// The reserved `cmd` that turns a connection into the long-lived event stream.
const SUBSCRIBE_EVENTS: &str = "__subscribe_events";

/// The largest frame we'll read — far above any legitimate poll batch, low
/// enough that a desync or hostile length can't make us allocate the moon.
const MAX_FRAME_LEN: usize = 256 * 1024 * 1024;

// ---- request / response / event shapes (mirror node_control) ---------------

/// One command over the control socket: a name plus its JSON args object.
#[derive(Debug, Serialize)]
struct NodeRequest {
    cmd: String,
    args: Value,
}

/// The body of a normal (`TAG_JSON`) response frame.
#[derive(Debug, Deserialize)]
struct WireResponse {
    ok: bool,
    #[serde(default)]
    result: Value,
    #[serde(default)]
    error: Option<String>,
}

/// One engine event as it travels the event connection.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NodeEvent {
    /// A `UiSink::emit` — one named event + its JSON payload.
    Emit { event: String, payload: Value },
    /// Desktop lifecycle signal; terminal clients ignore it.
    Upgrade,
    /// The node is re-execing onto a staged update.
    Restart,
}

// ---- framing ---------------------------------------------------------------

/// Write one length-prefixed frame: `[u32 BE len][tag][payload]`, then flush.
async fn write_frame<W: AsyncWrite + Unpin>(
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
/// clean EOF *before any byte of a frame* — a peer that hung up between frames.
async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Option<(u8, Vec<u8>)>> {
    let mut len_buf = [0u8; 4];
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

// ---- socket address (mirror node_control::node_socket_addr) -----------------

/// Where the node's control socket lives — distinct from the `myownmesh`
/// daemon socket. On unix it's `<myownmesh_home>/allmystuff-node.sock` (the
/// same `~/.myownmesh` home, honoring `MYOWNMESH_HOME`, the node uses); on
/// Windows it's a namespaced pipe.
enum SocketAddr {
    #[cfg(unix)]
    Path(std::path::PathBuf),
    #[cfg(not(unix))]
    Name(String),
}

#[cfg(not(unix))]
const NODE_SOCKET_NAME: &str = "allmystuff-node";

fn node_socket_addr() -> Result<SocketAddr, String> {
    #[cfg(unix)]
    {
        let home = allmystuff_protocol::myownmesh_state_dir().ok_or_else(|| {
            "couldn't resolve the ~/.myownmesh home for the node socket".to_string()
        })?;
        Ok(SocketAddr::Path(home.join("allmystuff-node.sock")))
    }
    #[cfg(not(unix))]
    {
        Ok(SocketAddr::Name(NODE_SOCKET_NAME.to_string()))
    }
}

impl SocketAddr {
    fn to_name(&self) -> Result<interprocess::local_socket::Name<'_>, String> {
        match self {
            #[cfg(unix)]
            SocketAddr::Path(p) => p
                .as_path()
                .to_fs_name::<GenericFilePath>()
                .map_err(|e| format!("node socket path → fs_name: {e}")),
            #[cfg(not(unix))]
            SocketAddr::Name(n) => n
                .as_str()
                .to_ns_name::<GenericNamespaced>()
                .map_err(|e| format!("node socket name → ns_name: {e}")),
        }
    }
}

// ---- the client ------------------------------------------------------------

/// Client of a running node's control socket. Cheap to construct; every call
/// opens its own connection (a local round trip is cheap, and pooling muddies
/// node-restart semantics — the same reasoning the node's own clients use).
pub struct NodeClient {
    addr: SocketAddr,
}

impl NodeClient {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            addr: node_socket_addr()?,
        })
    }

    async fn connect(&self) -> Result<LocalSocketStream, String> {
        let name = self.addr.to_name()?;
        LocalSocketStream::connect(name)
            .await
            .map_err(|e| format!("connect node socket: {e}"))
    }

    /// True when a node is already listening on the control socket.
    pub async fn probe() -> bool {
        match NodeClient::new() {
            Ok(c) => c.connect().await.is_ok(),
            Err(_) => false,
        }
    }

    async fn round_trip(&self, cmd: &str, args: Value) -> Result<(u8, Vec<u8>), String> {
        let stream = self.connect().await?;
        let (mut reader, mut writer) = stream.split();
        let body = serde_json::to_vec(&NodeRequest {
            cmd: cmd.to_string(),
            args,
        })
        .map_err(|e| e.to_string())?;
        write_frame(&mut writer, TAG_JSON, &body)
            .await
            .map_err(|e| format!("write node request: {e}"))?;
        read_frame(&mut reader)
            .await
            .map_err(|e| format!("read node response: {e}"))?
            .ok_or_else(|| "node closed the connection without a response".to_string())
    }

    /// One-shot command → JSON result (or the node's error string).
    pub async fn request(&self, cmd: &str, args: Value) -> Result<Value, String> {
        let (tag, payload) = self.round_trip(cmd, args).await?;
        if tag != TAG_JSON {
            return Err(format!("node sent a {tag} frame where JSON was expected"));
        }
        let resp: WireResponse =
            serde_json::from_slice(&payload).map_err(|e| format!("parse node response: {e}"))?;
        if resp.ok {
            Ok(resp.result)
        } else {
            Err(resp.error.unwrap_or_else(|| "(no error)".into()))
        }
    }

    /// One-shot command → raw bytes (the poll commands).
    pub async fn request_bytes(&self, cmd: &str, args: Value) -> Result<Vec<u8>, String> {
        let (tag, payload) = self.round_trip(cmd, args).await?;
        match tag {
            TAG_BYTES => Ok(payload),
            TAG_JSON => {
                let resp: WireResponse = serde_json::from_slice(&payload)
                    .map_err(|e| format!("parse node response: {e}"))?;
                Err(resp
                    .error
                    .unwrap_or_else(|| "node returned JSON where bytes were expected".into()))
            }
            other => Err(format!(
                "node sent a {other} frame where bytes were expected"
            )),
        }
    }

    /// Subscribe to the node's event stream: spawn a task forwarding each
    /// [`NodeEvent`] to `tx` until EOF. Returns once the ack lands.
    pub async fn subscribe_events(&self, tx: mpsc::Sender<NodeEvent>) -> Result<(), String> {
        let stream = self.connect().await?;
        let (mut reader, mut writer) = stream.split();
        let body = serde_json::to_vec(&NodeRequest {
            cmd: SUBSCRIBE_EVENTS.to_string(),
            args: Value::Null,
        })
        .map_err(|e| e.to_string())?;
        write_frame(&mut writer, TAG_JSON, &body)
            .await
            .map_err(|e| format!("write node subscribe: {e}"))?;

        let (tag, payload) = read_frame(&mut reader)
            .await
            .map_err(|e| format!("read subscribe ack: {e}"))?
            .ok_or_else(|| "node closed the connection before the subscribe ack".to_string())?;
        if tag != TAG_JSON {
            return Err("subscribe ack wasn't a JSON frame".into());
        }
        let ack: WireResponse =
            serde_json::from_slice(&payload).map_err(|e| format!("parse subscribe ack: {e}"))?;
        if !ack.ok {
            return Err(format!(
                "subscribe rejected: {}",
                ack.error.unwrap_or_else(|| "(no error)".into())
            ));
        }

        tokio::spawn(async move {
            // Keep the writer half alive for the read loop's lifetime.
            let _keepalive = writer;
            loop {
                match read_frame(&mut reader).await {
                    Ok(Some((TAG_EVENT, body))) => match serde_json::from_slice::<NodeEvent>(&body)
                    {
                        Ok(ev) => {
                            if tx.send(ev).await.is_err() {
                                break;
                            }
                        }
                        Err(_) => continue,
                    },
                    Ok(Some((TAG_RESTART, _))) => {
                        let _ = tx.send(NodeEvent::Restart).await;
                        break;
                    }
                    Ok(Some(_)) => continue,
                    Ok(None) | Err(_) => break,
                }
            }
        });
        Ok(())
    }
}

/// Wait until the node socket answers, or `timeout` elapses. Returns whether a
/// node is up.
pub async fn wait_for_socket(timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if NodeClient::probe().await {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}
