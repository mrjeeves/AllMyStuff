//! Client half of the `myownmesh` daemon's control protocol. The wire
//! types live in `allmystuff-protocol` (a mirror of the daemon's
//! `control.rs`); this module is just the transport — connect, write one
//! line, read one line — over the local socket (`interprocess`).
//!
//! Three shapes — the first two exactly like the MyOwnMesh GUI's client:
//!
//!  * [`ControlClient::request`] — short-lived round trip for every
//!    one-shot command.
//!  * [`ControlClient::subscribe_events`] — a long-lived stream that
//!    forwards each `ServerOut` line to a channel until the daemon
//!    disconnects.
//!  * [`MediaPipe`] — a long-lived *request* connection for the media
//!    plane, where per-send connect + round-trip would sit inside every
//!    frame.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use interprocess::local_socket::tokio::prelude::*;
#[cfg(unix)]
use interprocess::local_socket::GenericFilePath;
#[cfg(not(unix))]
use interprocess::local_socket::GenericNamespaced;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use allmystuff_protocol::control::{
    decode_inbound_frame, encode_media_frame, InboundFrame, MAX_MEDIA_FRAME_BYTES,
    MEDIA_KIND_AUDIO, MEDIA_KIND_VIDEO, MEDIA_KIND_VIDEO_DISCONTINUITY,
};
pub use allmystuff_protocol::{Request, Response};

/// Where the daemon's control socket lives. Recomputed locally (via the
/// protocol crate) so the GUI never has to link `myownmesh-core`.
enum SocketAddr {
    #[cfg(unix)]
    Path(std::path::PathBuf),
    #[cfg(not(unix))]
    Name(String),
}

pub struct ControlClient {
    addr: SocketAddr,
}

/// Local IPC is not a playout buffer. Four H.264 access units absorb ordinary
/// task scheduling jitter (about 67 ms at 60 fps / 133 ms at 30 fps) without
/// preserving seconds of history after the viewer stalls. Audio stays on its
/// own eight-packet queue so neither plane can head-of-line block the other.
pub(crate) const MEDIA_VIDEO_QUEUE_CAPACITY: usize = 4;
pub(crate) const MEDIA_AUDIO_QUEUE_CAPACITY: usize = 8;
pub(crate) type VideoFramingFn = dyn Fn(&str, u8) -> Option<bool> + Send + Sync;

/// One item on the bounded H.264 ingress queue. Paced fragments are reassembled
/// before this boundary, so a Frame is one complete access unit. A
/// discontinuity remains ordered beside the units it describes: reset streams
/// fence to a clean entry; gradual streams carry the next wave frame inline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InboundVideoEvent {
    Frame(InboundFrame),
    /// Framing could not be resolved before the authenticated lane binding
    /// arrived. Let the mesh perform its normal route fallback and paced
    /// decision; never guess that a paced fragment is a whole access unit.
    Unframed(InboundFrame),
    Discontinuity {
        from: String,
        stream: u8,
        entry: Option<InboundFrame>,
    },
}

/// Per peer/lane state for the daemon -> node handoff. The daemon's media pipe
/// must keep draining even if the node briefly falls behind: blocking here lets
/// the daemon's queue grow without bound and turns a live stream into replay.
/// Paced fragments assemble before the four-AU queue. Once a complete AU is
/// shed, reset streams suppress dependent deltas until a clean entry; gradual
/// streams order one gap marker and continue admitting convergence frames.
#[derive(Debug, Clone, Copy)]
struct InboundRecoveryState {
    mode: crate::video_wire::AuRecovery,
    marker_queued: bool,
}

const MAX_PACED_AU_CHUNKS: usize = 2048;
const MAX_PACED_AU_BYTES: usize = 16 * 1024 * 1024;
const MAX_PACED_AU_AGE: Duration = Duration::from_secs(1);

#[derive(Debug)]
struct InboundPacedAu {
    frame: InboundFrame,
    chunks: usize,
    updated: Instant,
}

#[derive(Default)]
struct InboundVideoFreshness {
    recovering: HashMap<(String, u8), InboundRecoveryState>,
    recovery: HashMap<(String, u8), crate::video_wire::AuRecovery>,
    paced: HashMap<(String, u8), InboundPacedAu>,
}

fn canonical_media_peer(id: &str) -> &str {
    if let Some((body, suffix)) = id.rsplit_once('-') {
        if suffix.len() == 5 && suffix.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return body;
        }
    }
    id
}

impl InboundVideoFreshness {
    fn lane(frame: &InboundFrame) -> (String, u8) {
        (canonical_media_peer(&frame.from).to_string(), frame.stream)
    }

    fn recovery_for(&self, lane: &(String, u8)) -> crate::video_wire::AuRecovery {
        self.recovery
            .get(lane)
            .copied()
            .unwrap_or(crate::video_wire::AuRecovery::Reset)
    }

    fn note_discontinuity(
        &mut self,
        from: String,
        stream: u8,
        tx: &mpsc::Sender<InboundVideoEvent>,
    ) -> bool {
        let lane = (canonical_media_peer(&from).to_string(), stream);
        let mode = self.recovery_for(&lane);
        let state = self
            .recovering
            .entry(lane.clone())
            .or_insert(InboundRecoveryState {
                mode,
                marker_queued: false,
            });
        state.mode = mode;
        if state.marker_queued {
            return true;
        }
        match tx.try_send(InboundVideoEvent::Discontinuity {
            from,
            stream,
            entry: None,
        }) {
            Ok(()) => {
                if mode == crate::video_wire::AuRecovery::Gradual {
                    self.recovering.remove(&lane);
                } else if let Some(state) = self.recovering.get_mut(&lane) {
                    state.marker_queued = true;
                }
                true
            }
            Err(mpsc::error::TrySendError::Full(_)) => true,
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    /// Assemble one negotiated paced-video fragment train before it reaches the
    /// AU queue. A large Game frame may contain dozens of fragments but still
    /// consumes exactly one queue slot when its count marker closes it.
    fn forward_paced(&mut self, frame: InboundFrame, tx: &mpsc::Sender<InboundVideoEvent>) -> bool {
        let lane = Self::lane(&frame);
        if let Some(expected) = crate::video::paced_au_marker_count(&frame.data) {
            let Some(pending) = self.paced.remove(&lane) else {
                return self.note_discontinuity(frame.from, frame.stream, tx);
            };
            if pending.frame.rtp_timestamp != frame.rtp_timestamp || pending.chunks != expected {
                return self.note_discontinuity(frame.from, frame.stream, tx);
            }
            return self.forward(pending.frame, tx);
        }

        let expired = self
            .paced
            .get(&lane)
            .is_some_and(|pending| pending.updated.elapsed() > MAX_PACED_AU_AGE);
        if expired {
            self.paced.remove(&lane);
        }
        let mut damaged = expired;
        match self.paced.get_mut(&lane) {
            Some(pending) if pending.frame.rtp_timestamp == frame.rtp_timestamp => {
                if pending.chunks >= MAX_PACED_AU_CHUNKS
                    || pending.frame.data.len().saturating_add(frame.data.len())
                        > MAX_PACED_AU_BYTES
                {
                    self.paced.remove(&lane);
                    return self.note_discontinuity(frame.from, frame.stream, tx);
                }
                pending.frame.key |= frame.key;
                pending.frame.data.extend_from_slice(&frame.data);
                pending.chunks += 1;
                pending.updated = Instant::now();
            }
            Some(_) => {
                self.paced.insert(
                    lane.clone(),
                    InboundPacedAu {
                        frame,
                        chunks: 1,
                        updated: Instant::now(),
                    },
                );
                damaged = true;
            }
            None => {
                self.paced.insert(
                    lane.clone(),
                    InboundPacedAu {
                        frame,
                        chunks: 1,
                        updated: Instant::now(),
                    },
                );
            }
        }
        if damaged {
            let (from, stream) = {
                let pending = self.paced.get(&lane).expect("paced frame just inserted");
                (pending.frame.from.clone(), pending.frame.stream)
            };
            self.note_discontinuity(from, stream, tx)
        } else {
            true
        }
    }

    fn discard_paced_lane(&mut self, frame: &InboundFrame) {
        self.paced.remove(&Self::lane(frame));
    }

    fn forward_transport_discontinuity(
        &mut self,
        frame: InboundFrame,
        tx: &mpsc::Sender<InboundVideoEvent>,
    ) -> bool {
        // Any paced fragments preceding the transport gap are necessarily an
        // incomplete AU. Drop them before ordering the decoder reset marker.
        self.discard_paced_lane(&frame);
        self.note_discontinuity(frame.from, frame.stream, tx)
    }

    /// Return false only when the consumer has gone away and the pipe should
    /// close. Queue pressure is handled locally and always returns true.
    fn forward(&mut self, frame: InboundFrame, tx: &mpsc::Sender<InboundVideoEvent>) -> bool {
        let lane = Self::lane(&frame);
        if let Some(identity) = crate::video_wire::peek_au_identity_marker(&frame.data) {
            self.recovery.insert(lane.clone(), identity.recovery);
            if let Some(state) = self.recovering.get_mut(&lane) {
                state.mode = identity.recovery;
            }
        }
        if let Some(state) = self.recovering.get(&lane).copied() {
            if state.mode == crate::video_wire::AuRecovery::Gradual {
                return match tx.try_send(InboundVideoEvent::Discontinuity {
                    from: frame.from.clone(),
                    stream: frame.stream,
                    entry: Some(frame),
                }) {
                    Ok(()) => {
                        self.recovering.remove(&lane);
                        true
                    }
                    Err(mpsc::error::TrySendError::Full(_)) => true,
                    Err(mpsc::error::TrySendError::Closed(_)) => false,
                };
            }
            if !state.marker_queued {
                let clean_entry = frame.key || crate::video_decode::is_decode_entry(&frame.data);
                let event = InboundVideoEvent::Discontinuity {
                    from: frame.from.clone(),
                    stream: frame.stream,
                    entry: clean_entry.then_some(frame),
                };
                return match tx.try_send(event) {
                    Ok(()) => {
                        if clean_entry {
                            self.recovering.remove(&lane);
                        } else if let Some(state) = self.recovering.get_mut(&lane) {
                            state.marker_queued = true;
                        }
                        true
                    }
                    Err(mpsc::error::TrySendError::Full(_)) => true,
                    Err(mpsc::error::TrySendError::Closed(_)) => false,
                };
            }

            if !(frame.key || crate::video_decode::is_decode_entry(&frame.data)) {
                return true;
            }
            return match tx.try_send(InboundVideoEvent::Frame(frame)) {
                Ok(()) => {
                    self.recovering.remove(&lane);
                    true
                }
                Err(mpsc::error::TrySendError::Full(_)) => true,
                Err(mpsc::error::TrySendError::Closed(_)) => false,
            };
        }

        match tx.try_send(InboundVideoEvent::Frame(frame)) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(InboundVideoEvent::Frame(frame))) => {
                let lane = Self::lane(&frame);
                let mode = self.recovery_for(&lane);
                self.recovering.insert(
                    lane,
                    InboundRecoveryState {
                        mode,
                        marker_queued: false,
                    },
                );
                true
            }
            Err(mpsc::error::TrySendError::Full(_)) => unreachable!("sent Frame"),
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }
}

impl ControlClient {
    pub fn new() -> Result<Self> {
        #[cfg(unix)]
        {
            let path = allmystuff_protocol::control::default_socket_path()
                .context("resolve daemon socket path")?;
            Ok(Self {
                addr: SocketAddr::Path(path),
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {
                addr: SocketAddr::Name(
                    allmystuff_protocol::control::default_pipe_name().to_string(),
                ),
            })
        }
    }

    /// A client for a daemon listening on an explicit socket path, for the
    /// one host that can't use the default: the mobile shell, whose sandbox
    /// forbids `$HOME`-root writes and whose container paths overrun the
    /// 104-byte `sun_path` limit — it parks the socket under the short
    /// `$TMPDIR` and hands the same path to the embedded daemon's config.
    #[cfg(unix)]
    pub fn with_path(path: std::path::PathBuf) -> Self {
        Self {
            addr: SocketAddr::Path(path),
        }
    }

    /// One-shot request → response. Opens a socket, writes one JSON line,
    /// reads one back, closes. No pooling (a local round trip is cheap and
    /// pooling muddies daemon-restart semantics).
    pub async fn request(&self, req: &Request) -> Result<Response> {
        self.request_with_timeout(req, Duration::from_secs(5)).await
    }

    /// [`Self::request`] with a caller-sized read deadline — for the ops
    /// whose reply legitimately takes longer than the 5 s default: a
    /// `NetworkConnectPeer { wait_ms, .. }` holding for ACTIVE, or a
    /// `ChannelSendReliable` holding for the peer's delivery ack. Size
    /// it past the op's own deadline (`wait_ms` / `ttl_ms`), never
    /// equal to it, so the daemon's honest timeout answer wins over the
    /// socket's.
    pub async fn request_with_timeout(
        &self,
        req: &Request,
        read_timeout: Duration,
    ) -> Result<Response> {
        let stream = self.connect().await?;
        let (reader, mut writer) = stream.split();
        let mut reader = BufReader::new(reader);

        let line = serde_json::to_string(req)? + "\n";
        writer
            .write_all(line.as_bytes())
            .await
            .context("write request")?;
        writer.flush().await.context("flush request")?;

        let mut buf = String::new();
        let n = tokio::time::timeout(read_timeout, reader.read_line(&mut buf))
            .await
            .context("daemon response timed out")??;
        if n == 0 {
            bail!("daemon closed the connection without a response");
        }
        serde_json::from_str(buf.trim()).with_context(|| format!("parse response: {buf}"))
    }

    /// Subscribe to the daemon's event stream. Forwards each line to `tx`
    /// as opaque JSON; returns after the initial ack.
    pub async fn subscribe_events(
        &self,
        tx: mpsc::Sender<serde_json::Value>,
    ) -> Result<allmystuff_protocol::ClientId> {
        let stream = self.connect().await?;
        let (reader, mut writer) = stream.split();
        let mut reader = BufReader::new(reader);

        let line = serde_json::to_string(&Request::EventsSubscribe)? + "\n";
        writer
            .write_all(line.as_bytes())
            .await
            .context("write subscribe")?;
        writer.flush().await.context("flush subscribe")?;

        let mut ack = String::new();
        let n = reader.read_line(&mut ack).await.context("read ack")?;
        if n == 0 {
            bail!("daemon closed the connection before the subscribe ack");
        }
        let parsed: Response =
            serde_json::from_str(ack.trim()).with_context(|| format!("parse ack: {ack}"))?;
        if !parsed.ok {
            return Err(anyhow!(
                "subscribe rejected: {}",
                parsed.error.unwrap_or_else(|| "(no error)".into())
            ));
        }
        // The ack carries this connection's client_id (as the daemon's
        // `c<n>` string); we pass it back on ChannelSubscribe so channel
        // frames route to this event socket.
        let client_id: allmystuff_protocol::ClientId = parsed
            .data
            .as_ref()
            .and_then(|d| d.get("client_id"))
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| anyhow!("subscribe ack missing client_id"))?;

        tokio::spawn(async move {
            // Keep the writer half alive for the lifetime of the read loop.
            let _writer_keepalive = writer;
            let mut buf = String::new();
            loop {
                buf.clear();
                match reader.read_line(&mut buf).await {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!("event stream read failed: {e}");
                        break;
                    }
                }
                let value: serde_json::Value = match serde_json::from_str(buf.trim()) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!("malformed event line: {e} — {buf}");
                        continue;
                    }
                };
                if tx.send(value).await.is_err() {
                    break;
                }
            }
        });

        Ok(client_id)
    }

    /// Open a dedicated binary **media-source** pipe for `client_id` (the id
    /// from [`subscribe_events`]). After the handshake the daemon pushes
    /// length-prefixed inbound media frames (`[u32 len][body]`) for everything
    /// that client subscribed to; this reads them and forwards each decoded
    /// [`InboundFrame`] to separate bounded video/audio queues. Inbound
    /// H.264/Opus then carries no base64. The reader never awaits a full media
    /// queue: audio sheds a packet, while H.264 enters an ordered clean-entry
    /// fence and keeps draining the daemon socket. The spawned reader ends when
    /// the daemon closes the pipe or a consumer is dropped; the caller can
    /// reconnect on the next session.
    ///
    /// [`subscribe_events`]: ControlClient::subscribe_events
    pub(crate) async fn subscribe_media_source(
        &self,
        client_id: allmystuff_protocol::ClientId,
        video_tx: mpsc::Sender<InboundVideoEvent>,
        audio_tx: mpsc::Sender<InboundFrame>,
        video_framing: Arc<VideoFramingFn>,
    ) -> Result<()> {
        let stream = self.connect().await?;
        let (reader, mut writer) = stream.split();
        let mut reader = BufReader::new(reader);

        let line = serde_json::to_string(&Request::MediaSourcePipe { client_id })? + "\n";
        writer
            .write_all(line.as_bytes())
            .await
            .context("write media-source handshake")?;
        writer
            .flush()
            .await
            .context("flush media-source handshake")?;

        let mut ack = String::new();
        let n = reader
            .read_line(&mut ack)
            .await
            .context("read media-source ack")?;
        if n == 0 {
            bail!("daemon closed the connection before the media-source ack");
        }
        let parsed: Response = serde_json::from_str(ack.trim())
            .with_context(|| format!("parse media-source ack: {ack}"))?;
        if !parsed.ok {
            return Err(anyhow!(
                "media-source rejected: {}",
                parsed.error.unwrap_or_else(|| "(no error)".into())
            ));
        }

        tokio::spawn(async move {
            // Hold the writer half open for the lifetime of the read loop
            // (dropping it would half-close the pipe).
            let _writer_keepalive = writer;
            let mut video_freshness = InboundVideoFreshness::default();
            loop {
                let mut len_buf = [0u8; 4];
                if reader.read_exact(&mut len_buf).await.is_err() {
                    break;
                }
                let len = u32::from_le_bytes(len_buf) as usize;
                if len > MAX_MEDIA_FRAME_BYTES {
                    tracing::warn!("media-source frame too large ({len} bytes) — closing pipe");
                    break;
                }
                let mut body = vec![0u8; len];
                if reader.read_exact(&mut body).await.is_err() {
                    break;
                }
                let Some(frame) = decode_inbound_frame(&body) else {
                    tracing::warn!("malformed media-source frame ({len} bytes) — skipped");
                    continue;
                };
                let keep_open = match frame.kind {
                    MEDIA_KIND_VIDEO => match video_framing(&frame.from, frame.stream) {
                        Some(true) => video_freshness.forward_paced(frame, &video_tx),
                        Some(false) => {
                            video_freshness.discard_paced_lane(&frame);
                            video_freshness.forward(frame, &video_tx)
                        }
                        None => match video_tx.try_send(InboundVideoEvent::Unframed(frame)) {
                            Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => true,
                            Err(mpsc::error::TrySendError::Closed(_)) => false,
                        },
                    },
                    MEDIA_KIND_VIDEO_DISCONTINUITY => {
                        video_freshness.forward_transport_discontinuity(frame, &video_tx)
                    }
                    MEDIA_KIND_AUDIO => match audio_tx.try_send(frame) {
                        Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => true,
                        Err(mpsc::error::TrySendError::Closed(_)) => false,
                    },
                    _ => true,
                };
                if !keep_open {
                    break;
                }
            }
        });

        Ok(())
    }

    async fn connect(&self) -> Result<LocalSocketStream> {
        let name = match &self.addr {
            #[cfg(unix)]
            SocketAddr::Path(p) => p
                .as_path()
                .to_fs_name::<GenericFilePath>()
                .context("socket path → fs_name")?,
            #[cfg(not(unix))]
            SocketAddr::Name(n) => n
                .as_str()
                .to_ns_name::<GenericNamespaced>()
                .context("socket name → ns_name")?,
        };
        LocalSocketStream::connect(name)
            .await
            .context("connect daemon socket — is `myownmesh serve` running?")
    }
}

/// A persistent connection dedicated to the media plane's sends.
///
/// [`ControlClient::request`]'s connect-send-await-close shape is right
/// for one-shot commands and wrong for a 24 fps frame stream: every ≤40 KiB
/// video chunk paid a socket connect plus a full round trip, *serially* —
/// several RTTs of dead air inside each frame. The daemon serves a
/// connection's request lines in order (`handle_client` loops), so this
/// pipe writes them back-to-back and drains the responses on a background
/// reader, which logs daemon refusals (rate-limited) instead of stalling
/// the send path to hear them.
///
/// Live video and audio writes have short freshness deadlines; other legacy
/// media requests retain the control-pipe deadline. Any timeout or write
/// failure drops the connection, and the next send reconnects.
pub struct MediaPipe {
    client: Arc<ControlClient>,
    writer: tokio::sync::Mutex<Option<interprocess::local_socket::tokio::SendHalf>>,
}

impl MediaPipe {
    pub fn new(client: Arc<ControlClient>) -> Self {
        MediaPipe {
            client,
            writer: tokio::sync::Mutex::new(None),
        }
    }

    /// Queue one request down the pipe, (re)connecting first if needed.
    /// `Ok` means the bytes reached the socket; the daemon's verdict
    /// arrives later via the reader task's (rate-limited) log line.
    pub async fn send(&self, req: &Request) -> Result<()> {
        let line = serde_json::to_string(req)? + "\n";
        let write_timeout = match req {
            Request::VideoSend { .. } => VIDEO_TRACK_WRITE_TIMEOUT,
            Request::AudioSend { .. } => AUDIO_TRACK_WRITE_TIMEOUT,
            _ => PIPE_WRITE_TIMEOUT,
        };
        let mut writer = self.writer.lock().await;
        if writer.is_none() {
            let stream = self.client.connect().await?;
            let (reader, send_half) = stream.split();
            spawn_response_drain(reader);
            *writer = Some(send_half);
        }
        let w = writer.as_mut().expect("connected above");
        // Bounded: a daemon that stops *reading* (wedged, not dead) never
        // errors the write — it just never completes, silently stalling
        // every media send behind this mutex forever. The timeout converts
        // that into the same drop-and-reconnect a write error gets. A
        // healthy local-socket write completes in microseconds; seconds of
        // blockage is a wedged peer, not backpressure.
        let outcome = tokio::time::timeout(write_timeout, async {
            w.write_all(line.as_bytes()).await?;
            w.flush().await
        })
        .await;
        match outcome {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => {
                *writer = None;
                Err(anyhow!("media pipe write: {e}"))
            }
            Err(_) => {
                *writer = None;
                Err(anyhow!(
                    "media pipe write timed out after {}ms — dropping the connection",
                    write_timeout.as_millis()
                ))
            }
        }
    }
}

/// How long one pipe write may block before the connection is declared
/// wedged and dropped for reconnect. Healthy local IPC flushes in
/// microseconds; genuine backpressure shows as *slow* progress, not a
/// multi-second single write.
const PIPE_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// A live media socket is a freshness boundary, not durable delivery. Local
/// IPC normally drains in microseconds; after these deadlines the encoded
/// unit is already stale enough that waiting longer only turns congestion into
/// visible input lag. Closing the pipe surfaces a send failure to the existing
/// mode-aware H.264 recovery epoch: reset encoders fence to a new IDR, while
/// GDR encoders keep the live edge moving through one bounded refresh wave.
const VIDEO_TRACK_WRITE_TIMEOUT: Duration = Duration::from_millis(200);
const AUDIO_TRACK_WRITE_TIMEOUT: Duration = Duration::from_millis(100);

/// A persistent connection dedicated to **binary** media-track sends — the
/// H.264 and Opus lanes. The first send converts the connection with a single
/// [`Request::MediaTrackPipe`] line; everything after is length-prefixed binary
/// frames (`[u32 len][body]`, see `allmystuff_protocol::encode_media_frame`) —
/// no base64 (+33% and a CPU pass) and no per-frame JSON of a multi-KB string.
/// MJPEG, PCM and route signalling stay on the JSON [`MediaPipe`], untouched.
/// Video and audio have independent connections and bounded freshness
/// deadlines, so a wedged track cannot head-of-line block the other. A timeout
/// or failed write drops that track's connection and the next send reconnects.
pub struct MediaTrackPipe {
    client: Arc<ControlClient>,
    writer: tokio::sync::Mutex<Option<interprocess::local_socket::tokio::SendHalf>>,
}

impl MediaTrackPipe {
    pub fn new(client: Arc<ControlClient>) -> Self {
        MediaTrackPipe {
            client,
            writer: tokio::sync::Mutex::new(None),
        }
    }

    /// Stream one H.264 access unit to `peer`'s video lane `stream`.
    pub async fn send_video(
        &self,
        network: &str,
        peer: &str,
        stream: u8,
        duration_us: u64,
        data: &[u8],
    ) -> Result<()> {
        self.send_frame(MEDIA_KIND_VIDEO, network, peer, stream, duration_us, data)
            .await
    }

    /// Stream one Opus frame to `peer`'s audio lane `stream`.
    pub async fn send_audio(
        &self,
        network: &str,
        peer: &str,
        stream: u8,
        duration_us: u64,
        data: &[u8],
    ) -> Result<()> {
        self.send_frame(MEDIA_KIND_AUDIO, network, peer, stream, duration_us, data)
            .await
    }

    async fn send_frame(
        &self,
        kind: u8,
        network: &str,
        peer: &str,
        stream: u8,
        duration_us: u64,
        data: &[u8],
    ) -> Result<()> {
        let body = encode_media_frame(kind, stream, duration_us, network, peer, data);
        let mut writer = self.writer.lock().await;
        if writer.is_none() {
            let conn = self.client.connect().await?;
            let (reader, mut send_half) = conn.split();
            spawn_response_drain(reader);
            // Convert the fresh connection to the binary media-track protocol.
            let line = serde_json::to_string(&Request::MediaTrackPipe)? + "\n";
            let hs = tokio::time::timeout(PIPE_WRITE_TIMEOUT, async {
                send_half.write_all(line.as_bytes()).await?;
                send_half.flush().await
            })
            .await;
            match hs {
                Ok(r) => r.context("media-track handshake")?,
                Err(_) => {
                    return Err(anyhow!(
                        "media-track handshake timed out after {}s",
                        PIPE_WRITE_TIMEOUT.as_secs()
                    ))
                }
            }
            *writer = Some(send_half);
        }
        // Header and body go out under one lock so frames never interleave.
        // Bounded like the JSON pipe: a hung-but-open daemon socket must
        // cost a reconnect, not a silent forever-stall of every audio and
        // video send behind this mutex (the one silent-freeze vector the
        // encoder pass left open).
        let w = writer.as_mut().expect("connected above");
        let len = (body.len() as u32).to_le_bytes();
        let write_timeout = match kind {
            MEDIA_KIND_VIDEO => VIDEO_TRACK_WRITE_TIMEOUT,
            MEDIA_KIND_AUDIO => AUDIO_TRACK_WRITE_TIMEOUT,
            _ => PIPE_WRITE_TIMEOUT,
        };
        let outcome = tokio::time::timeout(write_timeout, async {
            w.write_all(&len).await?;
            w.write_all(&body).await?;
            w.flush().await
        })
        .await;
        match outcome {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => {
                *writer = None;
                Err(anyhow!("media-track write: {e}"))
            }
            Err(_) => {
                *writer = None;
                Err(anyhow!(
                    "media-track write timed out after {}ms — dropping the connection",
                    write_timeout.as_millis()
                ))
            }
        }
    }
}

/// Drain one pipe connection's response lines, surfacing refusals. Media
/// send failures repeat at frame rate when a peer drops mid-stream, so
/// warnings are rate-limited; the task ends with its socket.
fn spawn_response_drain(reader: interprocess::local_socket::tokio::RecvHalf) {
    tokio::spawn(async move {
        const WARN_EVERY: Duration = Duration::from_secs(5);
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        let mut last_warn: Option<std::time::Instant> = None;
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let Ok(resp) = serde_json::from_str::<Response>(line.trim()) else {
                continue;
            };
            if !resp.ok && last_warn.is_none_or(|t| t.elapsed() >= WARN_EVERY) {
                last_warn = Some(std::time::Instant::now());
                tracing::warn!(
                    "media send refused by daemon: {}",
                    resp.error.unwrap_or_else(|| "(no error)".into())
                );
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video(from: &str, stream: u8, key: bool, timestamp: u32) -> InboundFrame {
        InboundFrame {
            kind: MEDIA_KIND_VIDEO,
            key,
            stream,
            rtp_timestamp: timestamp,
            from: from.to_string(),
            data: vec![timestamp as u8],
        }
    }

    fn identified_video(
        from: &str,
        stream: u8,
        key: bool,
        timestamp: u32,
        recovery: crate::video_wire::AuRecovery,
    ) -> InboundFrame {
        let mut frame = video(from, stream, key, timestamp);
        crate::video_wire::insert_au_identity_marker(
            &mut frame.data,
            crate::video_wire::AuIdentity {
                sequence: u64::from(timestamp),
                recovery,
            },
            false,
        );
        frame
    }

    #[test]
    fn video_pressure_orders_one_gap_and_suppresses_deltas_until_key() {
        let (tx, mut rx) = mpsc::channel(1);
        let mut gate = InboundVideoFreshness::default();

        assert!(gate.forward(video("peer", 0, false, 1), &tx));
        assert!(gate.forward(video("peer", 0, false, 2), &tx));
        assert!(matches!(
            rx.try_recv(),
            Ok(InboundVideoEvent::Frame(InboundFrame {
                rtp_timestamp: 1,
                ..
            }))
        ));

        // The first post-pressure delta becomes an ordered marker, not decoder
        // input. Further deltas disappear until a key is admitted.
        assert!(gate.forward(video("peer", 0, false, 3), &tx));
        assert!(matches!(
            rx.try_recv(),
            Ok(InboundVideoEvent::Discontinuity { entry: None, .. })
        ));
        assert!(gate.forward(video("peer", 0, false, 4), &tx));
        assert!(rx.try_recv().is_err());

        assert!(gate.forward(video("peer", 0, true, 5), &tx));
        assert!(matches!(
            rx.try_recv(),
            Ok(InboundVideoEvent::Frame(InboundFrame {
                key: true,
                rtp_timestamp: 5,
                ..
            }))
        ));
        assert!(gate.forward(video("peer", 0, false, 6), &tx));
        assert!(matches!(
            rx.try_recv(),
            Ok(InboundVideoEvent::Frame(InboundFrame {
                rtp_timestamp: 6,
                ..
            }))
        ));
    }

    #[test]
    fn transport_gap_discards_partial_paced_au_and_uses_existing_recovery_gate() {
        let (tx, mut rx) = mpsc::channel(MEDIA_VIDEO_QUEUE_CAPACITY);
        let mut gate = InboundVideoFreshness::default();

        let mut fragment = video("peer", 2, true, 90_000);
        fragment.data = vec![1, 2, 3];
        assert!(gate.forward_paced(fragment, &tx));
        assert!(!gate.paced.is_empty(), "partial paced AU is buffered");

        let gap = InboundFrame {
            kind: MEDIA_KIND_VIDEO_DISCONTINUITY,
            key: false,
            stream: 2,
            rtp_timestamp: 90_000,
            from: "peer".into(),
            data: Vec::new(),
        };
        assert!(gate.forward_transport_discontinuity(gap, &tx));
        assert!(gate.paced.is_empty(), "gap discards the partial paced AU");
        assert!(matches!(
            rx.try_recv(),
            Ok(InboundVideoEvent::Discontinuity {
                from,
                stream: 2,
                entry: None,
            }) if from == "peer"
        ));

        // Reset-mode recovery remains single-flight and suppresses deltas.
        assert!(gate.forward(video("peer", 2, false, 91_000), &tx));
        assert!(rx.try_recv().is_err());
        assert!(gate.forward(video("peer", 2, true, 92_000), &tx));
        assert!(matches!(
            rx.try_recv(),
            Ok(InboundVideoEvent::Frame(InboundFrame {
                key: true,
                stream: 2,
                rtp_timestamp: 92_000,
                ..
            }))
        ));
    }

    #[test]
    fn transport_gap_preserves_gradual_recovery_frames() {
        let (tx, mut rx) = mpsc::channel(MEDIA_VIDEO_QUEUE_CAPACITY);
        let mut gate = InboundVideoFreshness::default();
        let gradual = crate::video_wire::AuRecovery::Gradual;

        // Learn this lane's negotiated recovery behavior before the loss.
        assert!(gate.forward(identified_video("peer", 3, false, 1, gradual), &tx));
        assert!(matches!(
            rx.try_recv(),
            Ok(InboundVideoEvent::Frame(InboundFrame { stream: 3, .. }))
        ));

        let gap = InboundFrame {
            kind: MEDIA_KIND_VIDEO_DISCONTINUITY,
            key: false,
            stream: 3,
            rtp_timestamp: 2,
            from: "peer".into(),
            data: Vec::new(),
        };
        assert!(gate.forward_transport_discontinuity(gap, &tx));
        assert!(matches!(
            rx.try_recv(),
            Ok(InboundVideoEvent::Discontinuity {
                stream: 3,
                entry: None,
                ..
            })
        ));

        // Gradual mode must keep its convergence wave; only reset mode waits
        // for a key. The transport reports loss but never overrides this.
        assert!(gate.forward(identified_video("peer", 3, false, 3, gradual), &tx));
        assert!(matches!(
            rx.try_recv(),
            Ok(InboundVideoEvent::Frame(InboundFrame {
                key: false,
                stream: 3,
                rtp_timestamp: 3,
                ..
            }))
        ));
    }

    #[test]
    fn recovery_key_can_share_the_discontinuity_slot() {
        let (tx, mut rx) = mpsc::channel(1);
        let mut gate = InboundVideoFreshness::default();

        assert!(gate.forward(video("peer", 2, false, 1), &tx));
        assert!(gate.forward(video("peer", 2, false, 2), &tx));
        let _ = rx.try_recv();

        assert!(gate.forward(video("peer", 2, true, 3), &tx));
        assert!(matches!(
            rx.try_recv(),
            Ok(InboundVideoEvent::Discontinuity {
                stream: 2,
                entry: Some(InboundFrame {
                    key: true,
                    rtp_timestamp: 3,
                    ..
                }),
                ..
            })
        ));

        // The lane left recovery when the combined marker+key was admitted.
        assert!(gate.forward(video("peer", 2, false, 4), &tx));
        assert!(matches!(
            rx.try_recv(),
            Ok(InboundVideoEvent::Frame(InboundFrame {
                rtp_timestamp: 4,
                ..
            }))
        ));
    }

    #[test]
    fn pressure_recovery_is_scoped_to_one_peer_lane() {
        let (tx, mut rx) = mpsc::channel(1);
        let mut gate = InboundVideoFreshness::default();

        assert!(gate.forward(video("peer", 0, false, 1), &tx));
        assert!(gate.forward(video("peer", 1, false, 2), &tx));
        let _ = rx.try_recv();

        // Lane 1 is recovering, but lane 0 remains independently live.
        assert!(gate.forward(video("peer", 0, false, 3), &tx));
        assert!(matches!(
            rx.try_recv(),
            Ok(InboundVideoEvent::Frame(InboundFrame {
                stream: 0,
                rtp_timestamp: 3,
                ..
            }))
        ));
        assert!(gate.forward(video("peer", 1, false, 4), &tx));
        assert!(matches!(
            rx.try_recv(),
            Ok(InboundVideoEvent::Discontinuity {
                stream: 1,
                entry: None,
                ..
            })
        ));
    }

    #[test]
    fn recovery_lane_canonicalises_bare_and_display_peer_ids() {
        let (tx, mut rx) = mpsc::channel(1);
        let mut gate = InboundVideoFreshness::default();

        assert!(gate.forward(video("peer-AB12C", 0, false, 1), &tx));
        assert!(gate.forward(video("peer", 0, false, 2), &tx));
        let _ = rx.try_recv();
        assert!(gate.forward(video("peer-AB12C", 0, false, 3), &tx));
        assert!(matches!(
            rx.try_recv(),
            Ok(InboundVideoEvent::Discontinuity {
                from,
                stream: 0,
                entry: None,
            }) if from == "peer-AB12C"
        ));
    }

    #[test]
    fn parameter_set_led_entry_recovers_even_when_the_daemon_key_bit_is_false() {
        let (tx, mut rx) = mpsc::channel(1);
        let mut gate = InboundVideoFreshness::default();

        assert!(gate.forward(video("peer", 0, false, 1), &tx));
        assert!(gate.forward(video("peer", 0, false, 2), &tx));
        let _ = rx.try_recv();

        let mut hevc_entry = video("peer", 0, false, 3);
        hevc_entry.data = vec![0, 0, 0, 1, 0x40, 1];
        assert!(gate.forward(hevc_entry, &tx));
        assert!(matches!(
            rx.try_recv(),
            Ok(InboundVideoEvent::Discontinuity {
                entry: Some(InboundFrame {
                    key: false,
                    rtp_timestamp: 3,
                    ..
                }),
                ..
            })
        ));
    }

    #[test]
    fn a_closed_video_consumer_closes_the_media_pipe() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        assert!(!InboundVideoFreshness::default().forward(video("peer", 0, false, 1), &tx));
    }

    #[test]
    fn paced_frame_with_more_fragments_than_queue_slots_is_one_queue_item() {
        let (tx, mut rx) = mpsc::channel(MEDIA_VIDEO_QUEUE_CAPACITY);
        let mut gate = InboundVideoFreshness::default();
        let mut expected = Vec::new();

        for byte in 0..6u8 {
            let mut fragment = video("peer", 0, byte == 0, 90_000);
            fragment.data = vec![byte];
            expected.push(byte);
            assert!(gate.forward_paced(fragment, &tx));
            assert!(rx.try_recv().is_err(), "fragments stay before the AU queue");
        }
        let mut marker = video("peer", 0, false, 90_000);
        marker.data = crate::video::paced_au_marker(6);
        assert!(gate.forward_paced(marker, &tx));
        assert!(matches!(
            rx.try_recv(),
            Ok(InboundVideoEvent::Frame(InboundFrame {
                key: true,
                data,
                ..
            })) if data == expected
        ));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn gradual_queue_pressure_orders_one_gap_and_keeps_wave_frames() {
        let (tx, mut rx) = mpsc::channel(1);
        let mut gate = InboundVideoFreshness::default();

        assert!(gate.forward(
            identified_video("peer", 0, false, 1, crate::video_wire::AuRecovery::Gradual),
            &tx,
        ));
        assert!(gate.forward(
            identified_video("peer", 0, false, 2, crate::video_wire::AuRecovery::Gradual),
            &tx,
        ));
        let _ = rx.try_recv();

        assert!(gate.forward(
            identified_video("peer", 0, false, 3, crate::video_wire::AuRecovery::Gradual),
            &tx,
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(InboundVideoEvent::Discontinuity {
                entry: Some(InboundFrame {
                    key: false,
                    rtp_timestamp: 3,
                    ..
                }),
                ..
            })
        ));
    }
}
