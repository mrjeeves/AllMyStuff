//! Wayland screen capture with **restore tokens** — the portal dance
//! xcap runs, plus the one option it doesn't send.
//!
//! On Wayland the only sanctioned screen capture is the
//! `org.freedesktop.portal.ScreenCast` portal: ask for a session, let
//! the *user* pick what to share in the compositor's consent dialog,
//! receive a PipeWire node to pull frames from. xcap's recorder does
//! exactly that — but never sets `persist_mode`, so the dialog re-runs
//! on **every route start**, which is fatal for the one thing this app
//! exists to do: reach a machine nobody is sitting at.
//!
//! This module runs the same handshake itself and asks the portal to
//! persist the grant (`persist_mode = 2`, *until explicitly revoked*).
//! The `restore_token` that comes back is stored next to the app's
//! other state (`~/.myownmesh/allmystuff-screencast.json`) and replayed
//! on the next start: consent becomes a **once per machine** event, and
//! every start after it is silent and unattended. Tokens are single-use
//! — each `Start` response carries a fresh one, which replaces the
//! stored one (and a response with *no* token clears it, so a portal
//! that refused to persist isn't asked to restore garbage).
//!
//! The consent dialog is also why every portal wait here carries a
//! timeout: an unanswered dialog must degrade (the caller falls back to
//! per-frame grabs and tells the viewer in-band) rather than wedge the
//! capture thread — and with it, route teardown — forever. On timeout
//! the session is `Close`d so the compositor drops the stale dialog.
//!
//! Frames arrive on a dedicated PipeWire loop thread (format
//! negotiation and pixel conversion mirror xcap's recorder, with a
//! stride-aware copy), handed over an mpsc channel as ready-to-encode
//! RGBA. Dropping the [`WaylandSession`] quits the loop and joins the
//! thread, so a torn-down route releases its compositor stream.
//!
//! One honest limitation: the portal never lets an app *name* the
//! output it wants — the user picks in the dialog. A `screen:<id>`
//! route therefore keys its own token, and what it restores is whatever
//! the user picked for that tab the first time.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::Cursor;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use pipewire::{
    channel,
    context::ContextRc,
    keys::{MEDIA_CATEGORY, MEDIA_ROLE, MEDIA_TYPE},
    main_loop::MainLoopRc,
    properties,
    spa::{
        param::{
            format::{FormatProperties, MediaSubtype, MediaType},
            format_utils,
            video::{VideoFormat, VideoInfoRaw},
            ParamType,
        },
        pod::{self, serialize::PodSerializer, Pod},
        utils::{Direction, Fraction, Rectangle, SpaTypes},
    },
    stream::{StreamFlags, StreamRc},
};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{DeserializeDict, OwnedObjectPath, Type, Value};

/// One captured picture, packed RGBA — what the encoder pump wants.
pub struct RawFrame {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// The live capture: a compositor ScreenCast stream feeding the frame
/// channel. Dropping it quits the PipeWire loop, joins its thread, and only
/// then closes the portal session.
///
/// The `conn`/`session` are held for the capture's whole lifetime on purpose:
/// the portal ScreenCast session (and Mutter's PipeWire node behind it) lives
/// only as long as the D-Bus connection that created it. Letting them drop
/// when `open` returned closed the session out from under the freshly
/// connected stream — the node was being torn down while we negotiated, so
/// allocation/frames raced teardown and usually lost.
pub struct WaylandSession {
    conn: Connection,
    session: OwnedObjectPath,
    quit: Option<channel::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for WaylandSession {
    fn drop(&mut self) {
        if let Some(quit) = self.quit.take() {
            let _ = quit.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        // Close the portal session now the stream is done with its node —
        // not a moment before.
        close_session(&self.conn, &self.session);
    }
}

/// Whether a stored restore token exists for this monitor key — i.e.
/// whether the next [`open`] should start silently. The caller uses
/// this to tell the viewer a consent dialog is (probably) about to
/// need a human.
pub fn has_restore_token(monitor_id: Option<u32>) -> bool {
    load_token(&monitor_key(monitor_id)).is_some()
}

/// Drop the stored restore token for this monitor key, so the next
/// [`open`] re-prompts for fresh consent instead of replaying the saved
/// grant. Called when a restored session opened but never delivered a
/// frame: the token is pointing at an output the compositor no longer
/// paints, and replaying it would only strand the route again.
pub fn forget_token(monitor_id: Option<u32>) {
    save_token(&monitor_key(monitor_id), None);
}

/// Open a portal ScreenCast session (restoring a prior grant when a
/// token is stored) and start pulling frames from its PipeWire node.
pub fn open(monitor_id: Option<u32>) -> Result<(WaylandSession, Receiver<RawFrame>), String> {
    let key = monitor_key(monitor_id);
    let restore = load_token(&key);
    let restoring = restore.is_some();

    let conn = Connection::session().map_err(|e| format!("session bus: {e}"))?;
    let portal = screencast_proxy(&conn)?;

    let session = create_session(&conn, &portal)?;
    select_sources(&conn, &portal, &session, restore.as_deref())?;
    // The Start wait is the human one: with a token the portal answers
    // immediately; without one (or with one the compositor no longer
    // honours) the consent dialog is up until someone acts on it.
    let started = match start(&conn, &portal, &session, CONSENT_TIMEOUT) {
        Ok(s) => s,
        Err(e) => {
            close_session(&conn, &session);
            return Err(e);
        }
    };

    // Persist the rotated token — present means "replay me next time",
    // absent means the portal didn't persist the grant; never keep a
    // token the compositor has already burned.
    save_token(&key, started.restore_token.as_deref());
    if restoring && started.restore_token.is_none() {
        tracing::info!("screencast restore token not renewed — next start will ask consent");
    }

    let node_id = started
        .streams
        .as_ref()
        .and_then(|s| s.first())
        .map(|s| s.0)
        .ok_or("portal returned no stream")?;

    // The node the portal just minted lives on *its* PipeWire connection,
    // not the session daemon's default graph — most compositors (Mutter
    // included) only expose a screencast node over the fd handed back by
    // OpenPipeWireRemote. The consumer must connect to that fd; connecting to
    // the default daemon finds "no target node available" and the stream dies.
    let pw_fd = match open_pipewire_remote(&portal, &session) {
        Ok(fd) => fd,
        Err(e) => {
            close_session(&conn, &session);
            return Err(e);
        }
    };

    let (tx, rx) = std::sync::mpsc::channel::<RawFrame>();
    let (quit_tx, quit_rx) = channel::channel::<()>();
    let thread = std::thread::Builder::new()
        .name("wayland-screencast".into())
        .spawn(move || {
            if let Err(e) = pipewire_consume(node_id, pw_fd, tx, quit_rx) {
                tracing::warn!("wayland screencast pipewire loop ended: {e}");
            }
        })
        .map_err(|e| e.to_string())?;

    Ok((
        WaylandSession {
            // Held — not dropped — so the portal session and its node outlive
            // this call and stay up for the capture.
            conn,
            session,
            quit: Some(quit_tx),
            thread: Some(thread),
        },
        rx,
    ))
}

// ---- the portal handshake ----------------------------------------------

/// How long an unanswered consent dialog may hold a route start. Long
/// enough to walk to the machine; short enough that an unattended host
/// degrades to "tell the viewer" instead of wedging teardown.
const CONSENT_TIMEOUT: Duration = Duration::from_secs(120);
/// Configuration round-trips (no human involved) answer fast or never.
const PORTAL_TIMEOUT: Duration = Duration::from_secs(10);

const PORTAL_DEST: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";

#[derive(DeserializeDict, Type, Debug)]
#[zvariant(signature = "dict")]
struct CreateSessionResponse {
    session_handle: String,
}

#[derive(DeserializeDict, Type, Debug)]
#[zvariant(signature = "dict")]
struct SelectSourcesResponse {}

#[allow(dead_code)]
#[derive(DeserializeDict, Type, Debug)]
#[zvariant(signature = "dict")]
struct StartStream {
    id: Option<String>,
    position: Option<(i32, i32)>,
    size: Option<(i32, i32)>,
    source_type: Option<u32>,
    mapping_id: Option<String>,
}

#[derive(DeserializeDict, Type, Debug)]
#[zvariant(signature = "dict")]
struct StartResponse {
    streams: Option<Vec<(u32, StartStream)>>,
    restore_token: Option<String>,
}

fn screencast_proxy(conn: &Connection) -> Result<Proxy<'static>, String> {
    Proxy::new(
        conn,
        PORTAL_DEST,
        PORTAL_PATH,
        "org.freedesktop.portal.ScreenCast",
    )
    .map_err(|e| format!("ScreenCast portal: {e}"))
}

/// A unique-enough portal handle token: the portal only needs it to not
/// collide within our own connection.
fn handle_token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("allmystuff_{n}_{t}")
}

fn create_session(conn: &Connection, portal: &Proxy<'static>) -> Result<OwnedObjectPath, String> {
    let token = handle_token();
    let session_token = handle_token();
    let mut options: HashMap<&str, Value> = HashMap::new();
    options.insert("handle_token", Value::from(&token));
    options.insert("session_handle_token", Value::from(&session_token));

    let response: CreateSessionResponse = wait_response(conn, &token, PORTAL_TIMEOUT, || {
        portal
            .call_method("CreateSession", &(options))
            .map(|_| ())
            .map_err(|e| format!("CreateSession: {e}"))
    })?;

    OwnedObjectPath::try_from(response.session_handle).map_err(|e| format!("session handle: {e}"))
}

fn select_sources(
    conn: &Connection,
    portal: &Proxy<'static>,
    session: &OwnedObjectPath,
    restore_token: Option<&str>,
) -> Result<(), String> {
    let token = handle_token();
    let mut options: HashMap<&str, Value> = HashMap::new();
    options.insert("handle_token", Value::from(&token));
    options.insert("types", Value::from(1u32)); // MONITOR
    options.insert("multiple", Value::from(false));
    // 2 = persist until explicitly revoked — the whole point: consent
    // once, then silent unattended starts.
    options.insert("persist_mode", Value::from(2u32));
    if let Some(t) = restore_token {
        options.insert("restore_token", Value::from(t));
    }
    // Embed the host's pointer in the stream when the portal can — a
    // remote-control viewer steers that pointer, so seeing it matters.
    // (Bit 2 = EMBEDDED; the default elsewhere is HIDDEN.)
    if let Ok(modes) = portal.get_property::<u32>("AvailableCursorModes") {
        if modes & 2 != 0 {
            options.insert("cursor_mode", Value::from(2u32));
        }
    }

    let _: SelectSourcesResponse = wait_response(conn, &token, PORTAL_TIMEOUT, || {
        portal
            .call_method("SelectSources", &(session, options))
            .map(|_| ())
            .map_err(|e| format!("SelectSources: {e}"))
    })?;
    Ok(())
}

fn start(
    conn: &Connection,
    portal: &Proxy<'static>,
    session: &OwnedObjectPath,
    timeout: Duration,
) -> Result<StartResponse, String> {
    let token = handle_token();
    let mut options: HashMap<&str, Value> = HashMap::new();
    options.insert("handle_token", Value::from(&token));

    wait_response(conn, &token, timeout, || {
        portal
            .call_method("Start", &(session, "", options))
            .map(|_| ())
            .map_err(|e| format!("Start: {e}"))
    })
}

/// Ask the portal for the PipeWire connection the started session's node
/// lives on. Unlike the `Request`-based calls above this is a plain method
/// that returns a file descriptor directly (no `Response` signal): the
/// consumer connects its PipeWire context to this fd so the node id from
/// `Start` actually resolves. Returns an owned fd (zbus dups the received
/// one), ready to hand to `connect_fd_rc`.
fn open_pipewire_remote(
    portal: &Proxy<'static>,
    session: &OwnedObjectPath,
) -> Result<std::os::fd::OwnedFd, String> {
    let options: HashMap<&str, Value> = HashMap::new();
    let reply = portal
        .call_method("OpenPipeWireRemote", &(session, options))
        .map_err(|e| format!("OpenPipeWireRemote: {e}"))?;
    let fd: zbus::zvariant::OwnedFd = reply
        .body()
        .deserialize()
        .map_err(|e| format!("OpenPipeWireRemote reply: {e}"))?;
    Ok(fd.into())
}

/// Tell the portal we walked away, so a still-open consent dialog is
/// withdrawn instead of haunting the host's screen.
fn close_session(conn: &Connection, session: &OwnedObjectPath) {
    if let Ok(p) = Proxy::new(
        conn,
        PORTAL_DEST,
        session.as_str().to_owned(),
        "org.freedesktop.portal.Session",
    ) {
        let _ = p.call_method("Close", &());
    }
}

/// Subscribe to a portal request's `Response` signal *before* issuing
/// the method call, then wait for it with a timeout. The subscription
/// rides a helper thread so the wait can time out — a thread stuck on a
/// dialog nobody answers parks harmlessly until the portal closes the
/// request, instead of wedging the capture thread.
fn wait_response<T>(
    conn: &Connection,
    handle_token: &str,
    timeout: Duration,
    issue: impl FnOnce() -> Result<(), String>,
) -> Result<T, String>
where
    T: for<'de> serde::Deserialize<'de> + Type + Send + 'static,
{
    let unique = conn
        .unique_name()
        .ok_or("no unique bus name")?
        .trim_start_matches(':')
        .replace('.', "_");
    let request_path = format!("/org/freedesktop/portal/desktop/request/{unique}/{handle_token}");

    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let (out_tx, out_rx) = std::sync::mpsc::channel::<Result<T, String>>();
    let conn = conn.clone();
    std::thread::Builder::new()
        .name("portal-response".into())
        .spawn(move || {
            let subscribe = || -> Result<_, String> {
                let proxy = Proxy::new(
                    &conn,
                    PORTAL_DEST,
                    request_path,
                    "org.freedesktop.portal.Request",
                )
                .map_err(|e| e.to_string())?;
                proxy.receive_signal("Response").map_err(|e| e.to_string())
            };
            let mut signal = match subscribe() {
                Ok(s) => {
                    let _ = ready_tx.send(Ok(()));
                    s
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let outcome = (|| -> Result<T, String> {
                let message = signal.next().ok_or("portal request vanished")?;
                let body = message.body();
                let (code, body): (u32, T) = body.deserialize().map_err(|e| e.to_string())?;
                match code {
                    0 => Ok(body),
                    1 => Err("cancelled at the consent dialog".into()),
                    c => Err(format!("portal response code {c}")),
                }
            })();
            let _ = out_tx.send(outcome);
        })
        .map_err(|e| e.to_string())?;

    ready_rx
        .recv_timeout(PORTAL_TIMEOUT)
        .map_err(|_| "portal subscription stalled".to_string())??;
    issue()?;
    out_rx.recv_timeout(timeout).map_err(|_| {
        format!(
            "no portal answer within {}s (consent dialog unattended?)",
            timeout.as_secs()
        )
    })?
}

// ---- the PipeWire consumer ----------------------------------------------

#[derive(Clone)]
struct StreamData {
    format: VideoInfoRaw,
    /// Diagnostic: how many times the `process` callback has fired. Its
    /// staying at zero while the stream reports Streaming is the proof that
    /// the server never drove a single buffer to us.
    process_calls: u64,
}

/// Rate limit for the consumer's "why this frame was dropped" warns —
/// every drop condition repeats at frame rate; its explanation must not.
const DROP_WARN_EVERY: Duration = Duration::from_secs(5);

/// Per-condition rate-limited warns for the frame path. The capture
/// thread used to skip bad frames silently, which from the far end reads
/// as "session started, then nothing" — indistinguishable from a dark
/// display. One line per condition per window names the real problem.
struct DropWarns(HashMap<&'static str, Instant>);

impl DropWarns {
    fn new() -> Self {
        DropWarns(HashMap::new())
    }

    fn warn(&mut self, key: &'static str, msg: impl FnOnce() -> String) {
        let now = Instant::now();
        let due = self
            .0
            .get(key)
            .is_none_or(|t| now.duration_since(*t) >= DROP_WARN_EVERY);
        if due {
            self.0.insert(key, now);
            tracing::warn!("{}", msg());
        }
    }
}

/// The buffer parameters the consumer offers after a format is negotiated,
/// as a `SPA_TYPE_OBJECT_ParamBuffers` pod for
/// [`pipewire::stream::Stream::update_params`].
///
/// `dataType` accepts every memory type — MemFd/MemPtr **and** DMA-BUF.
/// GNOME 50's Mutter is DMA-BUF-first for screencast: a mappable-only request
/// it can't satisfy dead-ends the buffer negotiation and it allocates nothing
/// (the negotiate-then-frameless stall we traced). Accept its native buffers
/// so allocation completes; `MAP_BUFFERS` maps what it can and `add_buffer`
/// reports what we actually got. `size`/`stride` are geometry hints; the real
/// per-frame stride is still read from the buffer chunk.
fn shm_buffers_pod(width: u32, height: u32, bpp: u32) -> Result<Vec<u8>, String> {
    use pipewire::spa::sys;
    use pipewire::spa::utils::{Choice, ChoiceEnum, ChoiceFlags};

    let stride = (width * bpp) as i32;
    let size = stride * height as i32;
    let shm = (1i32 << sys::SPA_DATA_MemFd)
        | (1i32 << sys::SPA_DATA_MemPtr)
        | (1i32 << sys::SPA_DATA_DmaBuf);
    let obj = pod::Object {
        type_: SpaTypes::ObjectParamBuffers.as_raw(),
        id: ParamType::Buffers.as_raw(),
        properties: vec![
            pod::Property {
                key: sys::SPA_PARAM_BUFFERS_buffers,
                flags: pod::PropertyFlags::empty(),
                value: pod::Value::Choice(pod::ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: 8,
                        min: 1,
                        max: 32,
                    },
                ))),
            },
            pod::Property::new(sys::SPA_PARAM_BUFFERS_blocks, pod::Value::Int(1)),
            pod::Property::new(sys::SPA_PARAM_BUFFERS_size, pod::Value::Int(size)),
            pod::Property::new(sys::SPA_PARAM_BUFFERS_stride, pod::Value::Int(stride)),
            pod::Property {
                key: sys::SPA_PARAM_BUFFERS_dataType,
                flags: pod::PropertyFlags::empty(),
                value: pod::Value::Choice(pod::ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Flags {
                        default: shm,
                        flags: vec![],
                    },
                ))),
            },
        ],
    };
    Ok(
        PodSerializer::serialize(Cursor::new(Vec::new()), &pod::Value::Object(obj))
            .map_err(|e| e.to_string())?
            .0
            .into_inner(),
    )
}

/// Connect to the portal's stream node and pump pictures into `tx`
/// until the quit channel fires. Format negotiation and conversion
/// mirror xcap's recorder (RGB/RGBA/RGBx/BGRx → packed RGBA), plus a
/// stride-aware copy — compositors pad rows on some resolutions.
fn pipewire_consume(
    node_id: u32,
    pw_fd: std::os::fd::OwnedFd,
    tx: Sender<RawFrame>,
    quit: channel::Receiver<()>,
) -> Result<(), String> {
    pipewire::init();

    let main_loop = MainLoopRc::new(None).map_err(|e| e.to_string())?;
    let context = ContextRc::new(&main_loop, None).map_err(|e| e.to_string())?;
    // Connect to the portal's PipeWire remote (the fd from OpenPipeWireRemote),
    // not the default daemon — that's the only graph the screencast node is on.
    let core = context
        .connect_fd_rc(pw_fd, None)
        .map_err(|e| e.to_string())?;

    // The portal's node propagates onto this fresh connection asynchronously,
    // at a variable time. Binding the stream before it lands races it and
    // dies "no target node available" (or the negotiation stalls). A single
    // sync roundtrip isn't enough — the node can register a beat late. So
    // watch the registry and wait for *this* node id to actually appear before
    // touching the stream, re-syncing a bounded number of times. If it never
    // shows, bind anyway rather than hang.
    {
        let registry = core.get_registry_rc().map_err(|e| e.to_string())?;
        let found = Rc::new(Cell::new(false));
        let _reg = registry
            .add_listener_local()
            .global({
                let found = found.clone();
                let main_loop = main_loop.clone();
                move |g| {
                    if g.id == node_id {
                        found.set(true);
                        main_loop.quit();
                    }
                }
            })
            .register();
        let _core = core
            .add_listener_local()
            .done({
                let main_loop = main_loop.clone();
                move |_, _| main_loop.quit()
            })
            .register();
        for _ in 0..40 {
            if found.get() {
                break;
            }
            let _ = core.sync(0);
            main_loop.run();
        }
        if !found.get() {
            tracing::warn!(
                "wayland screencast node {node_id} never appeared in the registry; binding anyway"
            );
        }
    }

    let stream = StreamRc::new(
        core.clone(),
        "AllMyStuff",
        properties::properties! {
            *MEDIA_TYPE => "Video",
            *MEDIA_CATEGORY => "Capture",
            *MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|e| e.to_string())?;

    // A stream that dies (compositor revoked the grant, the output it
    // recorded went away, negotiation failed) raises no panic and sends
    // no frame — it just changes state. Surface that as the loop's
    // result so the capture thread can fall back instead of idling on a
    // dead stream forever.
    let stream_error: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

    let mut drops = DropWarns::new();
    let _listener = stream
        .add_local_listener_with_user_data(StreamData {
            format: Default::default(),
            process_calls: 0,
        })
        .state_changed({
            let main_loop = main_loop.clone();
            let stream_error = stream_error.clone();
            move |_, _, old, new| {
                if let pipewire::stream::StreamState::Error(e) = &new {
                    *stream_error.borrow_mut() = Some(e.clone());
                    main_loop.quit();
                } else {
                    // At info while we chase the frameless-Mutter case: the
                    // Paused→Streaming transition (or the lack of it) is the
                    // tell for whether buffers ever started flowing.
                    tracing::info!("wayland screencast stream: {old:?} → {new:?}");
                }
            }
        })
        .add_buffer(|_, _, buffer| {
            // The decisive probe for "negotiated but frameless": what kind of
            // buffer did the compositor allocate? spa_data type 1=MemPtr,
            // 2=MemFd (both mappable, our read path), 3=DmaBuf (Mutter's
            // default, which MAP_BUFFERS can't hand us as CPU pixels).
            let dtype = unsafe {
                let buf = (*buffer).buffer;
                if buf.is_null() || (*buf).n_datas == 0 {
                    None
                } else {
                    Some((*(*buf).datas).type_)
                }
            };
            tracing::info!("wayland screencast buffer allocated: spa_data type {dtype:?}");
        })
        .param_changed(|stream, data, id, param| {
            let Some(param) = param else { return };
            // Trace the negotiation: a Buffers param coming back after Format
            // means the server is driving allocation; silence after Format
            // means it's stuck on the buffer step.
            tracing::info!("wayland screencast param_changed id={id}");
            if id != ParamType::Format.as_raw() {
                return;
            }
            let (media_type, media_subtype) = match format_utils::parse_format(param) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("screencast format parse: {e:?}");
                    return;
                }
            };
            if media_type != MediaType::Video || media_subtype != MediaSubtype::Raw {
                return;
            }
            if let Err(e) = data.format.parse(param) {
                tracing::warn!("screencast format parse: {e:?}");
                return;
            }
            let size = data.format.size();
            let fmt = data.format.format();
            tracing::info!(
                "wayland screencast negotiated: {fmt:?} {}×{}",
                size.width,
                size.height
            );
            // Answer with SHM buffer params so the server allocates
            // CPU-readable buffers (see `shm_buffers_pod`) — Mutter otherwise
            // sticks on DMA-BUF we can't take, and never allocates at all.
            let bpp = match fmt {
                VideoFormat::RGB => 3,
                _ => 4,
            };
            match shm_buffers_pod(size.width, size.height, bpp) {
                Ok(bytes) => match Pod::from_bytes(&bytes) {
                    Some(pod) => {
                        if let Err(e) = stream.update_params(&mut [pod]) {
                            tracing::warn!("screencast update_params(buffers): {e}");
                        }
                    }
                    None => tracing::warn!("screencast buffers pod invalid"),
                },
                Err(e) => tracing::warn!("screencast buffers pod: {e}"),
            }
        })
        .process(move |stream, data| {
            data.process_calls += 1;
            let n = data.process_calls;
            let Some(mut buffer) = stream.dequeue_buffer() else {
                // Proves `process` is firing even when there's nothing to
                // take — distinguishes "never streaming" from "streaming but
                // starved".
                if n <= 8 || n % 60 == 0 {
                    tracing::info!("screencast process #{n}: woke with no buffer to dequeue");
                }
                return;
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                if n <= 8 {
                    tracing::info!("screencast process #{n}: buffer with zero data planes");
                }
                drops.warn("planes", || {
                    "screencast buffer carried no data planes — frame dropped".into()
                });
                return;
            }
            // Loud first-frames diagnostic: what the server actually handed us
            // on the first few process calls (plane count, chunk size, whether
            // the data mapped). Silence of *all* "process #" lines means the
            // server never drove a buffer despite reporting Streaming.
            if n <= 8 {
                let chunk = datas[0].chunk().size();
                let mapped = datas[0].data().is_some();
                tracing::info!(
                    "screencast process #{n}: {} plane(s), chunk {chunk} bytes, mapped={mapped}",
                    datas.len()
                );
            }
            let size = data.format.size();
            let (w, h) = (size.width, size.height);
            if w == 0 || h == 0 {
                drops.warn("no-format", || {
                    "screencast frame arrived before format negotiation — dropped".into()
                });
                return;
            }
            let format = data.format.format();
            let bpp: usize = match format {
                VideoFormat::RGB => 3,
                VideoFormat::RGBA | VideoFormat::RGBx | VideoFormat::BGRx | VideoFormat::BGRA => 4,
                other => {
                    drops.warn("format", || {
                        format!("screencast format {other:?} unsupported — frame dropped")
                    });
                    return;
                }
            };
            let stride = {
                let s = datas[0].chunk().stride();
                if s > 0 {
                    s as usize
                } else {
                    w as usize * bpp
                }
            };
            let Some(frame_data) = datas[0].data() else {
                drops.warn("unmappable", || {
                    "screencast buffer not mappable (DMA-BUF only?) — frame dropped".into()
                });
                return;
            };
            // Pack the rows (drop any stride padding), then normalize to
            // RGBA exactly the way xcap's recorder does.
            let row = w as usize * bpp;
            let mut packed = Vec::with_capacity(row * h as usize);
            for y in 0..h as usize {
                let start = y * stride;
                let Some(src) = frame_data.get(start..start + row) else {
                    drops.warn("torn", || {
                        format!(
                            "screencast buffer shorter than {w}×{h} at stride {stride} — \
                             torn frame dropped"
                        )
                    });
                    return;
                };
                packed.extend_from_slice(src);
            }
            let rgba = match format {
                VideoFormat::RGB => {
                    let mut buf = vec![0u8; (w * h * 4) as usize];
                    for (src, dst) in packed.chunks_exact(3).zip(buf.chunks_exact_mut(4)) {
                        dst[..3].copy_from_slice(src);
                        dst[3] = 255;
                    }
                    buf
                }
                VideoFormat::BGRx | VideoFormat::BGRA => {
                    let mut buf = packed;
                    for px in buf.chunks_exact_mut(4) {
                        px.swap(0, 2);
                    }
                    buf
                }
                _ => packed, // RGBA / RGBx
            };
            let _ = tx.send(RawFrame {
                rgba,
                width: w,
                height: h,
            });
        })
        .register()
        .map_err(|e| e.to_string())?;

    let obj = pod::object!(
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::RGB,
            VideoFormat::RGBA,
            VideoFormat::RGBx,
            VideoFormat::BGRx,
            // KWin (and Mutter on some stacks) offers BGRA first for
            // shm screen casts; without it the intersection can come
            // up empty and the stream dies before its first frame.
            VideoFormat::BGRA,
        ),
        pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle {
                width: 128,
                height: 128
            },
            Rectangle {
                width: 1,
                height: 1
            },
            Rectangle {
                width: 8192,
                height: 8192
            }
        ),
        pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: 30, denom: 1 },
            Fraction { num: 0, denom: 1 },
            Fraction {
                num: 1000,
                denom: 1
            }
        ),
    );
    let values = PodSerializer::serialize(Cursor::new(Vec::new()), &pod::Value::Object(obj))
        .map_err(|e| e.to_string())?
        .0
        .into_inner();
    let mut params = [Pod::from_bytes(&values).ok_or("failed to build format pod")?];

    stream
        .connect(
            Direction::Input,
            Some(node_id),
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|e| e.to_string())?;

    let _attached = quit.attach(main_loop.loop_(), {
        let main_loop = main_loop.clone();
        move |_| main_loop.quit()
    });

    main_loop.run();
    // A loop quit by the error listener (vs. the route's own quit
    // signal) ends the consumer with the compositor's reason; the
    // dropped frame channel then bounces the capture thread onto its
    // fallback path immediately instead of after the stall deadline.
    if let Some(e) = stream_error.borrow_mut().take() {
        return Err(format!("stream error: {e}"));
    }
    Ok(())
}

// ---- restore-token persistence -------------------------------------------

fn monitor_key(monitor_id: Option<u32>) -> String {
    match monitor_id {
        Some(id) => format!("monitor:{id}"),
        None => "primary".to_string(),
    }
}

/// Token file next to the app's other state (the ownership store keeps
/// the same home: `MYOWNMESH_HOME` override, else `~`).
fn token_store_path() -> Option<PathBuf> {
    Some(allmystuff_protocol::myownmesh_state_dir()?.join("allmystuff-screencast.json"))
}

fn load_token(key: &str) -> Option<String> {
    read_tokens(&token_store_path()?).remove(key)
}

fn save_token(key: &str, token: Option<&str>) {
    let Some(path) = token_store_path() else {
        return;
    };
    let mut tokens = read_tokens(&path);
    match token {
        Some(t) => {
            tokens.insert(key.to_string(), t.to_string());
        }
        None => {
            tokens.remove(key);
        }
    }
    write_tokens(&path, &tokens);
}

fn read_tokens(path: &std::path::Path) -> HashMap<String, String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn write_tokens(path: &std::path::Path, tokens: &HashMap<String, String>) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match serde_json::to_string_pretty(tokens) {
        Ok(json) => {
            if let Err(e) = std::fs::write(path, json) {
                tracing::warn!("couldn't persist screencast token: {e}");
            }
        }
        Err(e) => tracing::warn!("couldn't serialize screencast tokens: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_round_trip_per_monitor_key() {
        let dir = std::env::temp_dir().join(format!("ams-vstat-test-{}", std::process::id()));
        let path = dir.join("tokens.json");
        let mut tokens = HashMap::new();
        tokens.insert(monitor_key(None), "tok-primary".to_string());
        tokens.insert(monitor_key(Some(7)), "tok-7".to_string());
        write_tokens(&path, &tokens);
        let back = read_tokens(&path);
        assert_eq!(back.get("primary").map(String::as_str), Some("tok-primary"));
        assert_eq!(back.get("monitor:7").map(String::as_str), Some("tok-7"));
        // Clearing a key (a Start with no renewed token) removes it.
        let mut cleared = back;
        cleared.remove(&monitor_key(Some(7)));
        write_tokens(&path, &cleared);
        assert!(!read_tokens(&path).contains_key("monitor:7"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn handle_tokens_never_collide_in_process() {
        let a = handle_token();
        let b = handle_token();
        assert_ne!(a, b);
        assert!(a.starts_with("allmystuff_"));
    }
}
