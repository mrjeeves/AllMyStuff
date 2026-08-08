//! Mesh-native terminal sessions — the backend of "Open Terminal".
//!
//! Two halves, one struct:
//!
//!  * **Host** (the machine whose shell runs): a shell is a first-class
//!    **session** keyed by a `session_id` that any number of viewers
//!    ("attachers", keyed by `route_id`) attach to — the tmux model. A
//!    session opens a real PTY (openpty on Unix, ConPTY on Windows —
//!    `portable-pty` picks at runtime) and runs this user's shell in it.
//!    Three small blocking threads own the blocking ends — reader, control
//!    (writer + resize + kill), and wait (authoritative exit) — and meet the
//!    async world over a per-session [`tokio::sync::broadcast`] so PTY output
//!    fans out to every attacher. The reader also keeps a bounded scrollback
//!    ring so a fresh attach paints the current screen.
//!  * **Viewer** (the machine looking at it): inbound output is buffered
//!    per route and pulled by the terminal window with the same
//!    poke-then-pull watcher pattern the video plane uses — a lost "ready"
//!    poke costs latency, never bytes.
//!
//! No sshd, no credentials: the mesh already proved who the peer is, and
//! the caller gates everything on the owner/fleet rule before any of this
//! runs.

use std::collections::HashMap;
use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, PtySize};

use crate::byte_queues::ByteQueues;

/// What a hosted PTY produces for the mesh pump. `Clone` so one shell's
/// output can fan out to every attacher (broadcast) — the basis for
/// tmux-style shared terminals.
#[derive(Debug, Clone)]
pub enum OutMsg {
    /// A chunk of PTY output (≤ [`READ_BUF`] bytes).
    Data(Vec<u8>),
    /// The shared PTY's reconciled size changed — every attacher renders its
    /// emulator at this authoritative size (letterboxing a bigger window), so
    /// the wrapping matches the one shell for everyone. Broadcast whenever an
    /// attach/detach/resize moves the reconciled (minimum) size.
    Resize { cols: u16, rows: u16 },
    /// The shell ended (`None` = killed by signal / no status).
    Exit(Option<i32>),
}

/// What the mesh feeds a hosted PTY.
enum CtlMsg {
    Data(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Shutdown,
}

/// One PTY read at a time — small enough that a slow viewer throttles the
/// shell quickly, big enough that `cat bigfile` isn't syscall-bound.
const READ_BUF: usize = 8 * 1024;
/// Broadcast slots in flight host-side before the slowest attacher starts
/// dropping (lagging) — output is live media, a stalled attacher must never
/// wedge the shell or the other attachers.
const OUT_QUEUE: usize = 256;
/// Keystrokes/resizes queued before writes are dropped (a shell wedged in
/// flow-stop shouldn't stall the shared mesh loop).
const CTL_QUEUE: usize = 256;
/// A viewer window that never drains caps its buffer here; beyond it the
/// oldest chunks go (the terminal is live media, not a transcript).
const MAX_QUEUED_BYTES: usize = 4 * 1024 * 1024;
/// Recent PTY output kept per session and replayed to a fresh attach so it
/// paints the current screen — a screenful of scrollback, not a transcript.
const SCROLLBACK_CAP: usize = 256 * 1024;
/// After the last attacher detaches, how long a session lingers before the
/// idle reaper kills it — generous, so a flaky viewer or a quick re-attach
/// from another machine never loses a working shell. One hour.
const SESSION_IDLE_REAP_MS: u64 = 60 * 60 * 1000;

/// AMS-06: the most concurrent PTYs this host will spawn. A generous ceiling,
/// far above any legitimate multi-viewer use, that stops an authorised-but-
/// abusive controller from spawning unbounded shells (a local fork-bomb of
/// `bash` processes). Attaching to an existing session is never capped.
const MAX_LOCAL_SESSIONS: usize = 32;

/// A bounded byte ring of recent PTY output. Cheap to append, snapshots to a
/// contiguous `Vec<u8>` for replay; the `parking_lot::Mutex` around it is the
/// one guard the reader appends-and-broadcasts under, so an attacher that
/// snapshots-then-subscribes under the same guard gets a clean split.
struct Scrollback {
    buf: std::collections::VecDeque<u8>,
    cap: usize,
}

impl Scrollback {
    fn new(cap: usize) -> Self {
        Scrollback {
            buf: std::collections::VecDeque::new(),
            cap,
        }
    }

    fn append(&mut self, bytes: &[u8]) {
        // A chunk larger than the cap: keep only its tail.
        if bytes.len() >= self.cap {
            self.buf.clear();
            self.buf.extend(&bytes[bytes.len() - self.cap..]);
            return;
        }
        self.buf.extend(bytes);
        while self.buf.len() > self.cap {
            self.buf.pop_front();
        }
    }

    fn snapshot(&self) -> Vec<u8> {
        self.buf.iter().copied().collect()
    }
}

/// What one attacher (route) contributes to a session: the emulator size we
/// reconcile against so no attacher's screen overflows the shared PTY.
#[derive(Clone, Copy)]
struct Attacher {
    cols: u16,
    rows: u16,
}

struct PtySession {
    ctl_tx: std::sync::mpsc::SyncSender<CtlMsg>,
    /// Kills the child directly even when the control thread is busy
    /// mid-write — close/reap must never wait on a wedged shell.
    killer: Box<dyn ChildKiller + Send + Sync>,
    /// One sender; every attacher holds a [`broadcast::Receiver`] of it.
    out_tx: tokio::sync::broadcast::Sender<OutMsg>,
    /// Recent output, replayed on attach. The reader appends and broadcasts
    /// under this one guard so snapshot-then-subscribe never gaps or dups.
    scrollback: Arc<Mutex<Scrollback>>,
    /// Routes currently attached and the size each wants — reconciled to the
    /// per-dimension minimum on every resize.
    attachers: HashMap<String, Attacher>,
    /// Bumped on every (re)create of this id; the idle reaper only kills if
    /// the generation it armed against is still current, so a re-open that
    /// re-used a recycled id is never reaped by a stale timer.
    generation: u64,
    title: String,
    created_unix: u64,
    /// The reconciled size last pushed to the PTY and broadcast to attachers,
    /// so a no-op reconcile doesn't re-broadcast (and the viewers letterbox to
    /// a size that only changes when it really does).
    last_size: (u16, u16),
}

impl PtySession {
    /// Resize the shared PTY to the reconciled (minimum) size of all current
    /// attachers and, when that size actually changed, tell every attacher so
    /// each renders its emulator at the one shared size (letterboxing a bigger
    /// window). Returns whether the PTY control send succeeded.
    fn reconcile(&mut self) -> bool {
        let (cols, rows) = reconcile_size(&self.attachers);
        let ok = self.ctl_tx.try_send(CtlMsg::Resize { cols, rows }).is_ok();
        if (cols, rows) != self.last_size {
            self.last_size = (cols, rows);
            // Fire-and-forget: a lagging attacher catches the next one, and
            // "no receivers yet" is fine (the next attach reconciles anew).
            let _ = self.out_tx.send(OutMsg::Resize { cols, rows });
        }
        ok
    }
}

/// The handle [`TerminalHost::open`] hands back: the live output stream plus
/// everything an attacher needs to paint immediately.
pub struct TermAttach {
    pub session_id: String,
    /// Everything broadcast before this attach subscribed — replay it first,
    /// then drain `rx`, for a gapless, dup-free screen.
    pub scrollback: Vec<u8>,
    pub rx: tokio::sync::broadcast::Receiver<OutMsg>,
    /// `true` when this call created the session, `false` when it attached to
    /// an existing one.
    pub created: bool,
}

/// A row in [`TerminalHost::list_sessions`] — the shape the picker UI wants.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub title: String,
    pub created_unix: u64,
    pub attachers: usize,
}

type Sessions = Arc<Mutex<HashMap<String, PtySession>>>;

pub struct TerminalHost {
    sessions: Sessions,
    /// route_id → session_id, so route-keyed input/resize/detach find their
    /// session (and one route maps to exactly one session at a time).
    route_to_session: Mutex<HashMap<String, String>>,
    /// Mints session ids when the caller doesn't supply one.
    next_session: AtomicU64,
    /// Viewer-side buffers of PTY output per route, drained by the
    /// terminal window (the shared poke-then-pull queue plumbing).
    output: ByteQueues,
}

impl Default for TerminalHost {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalHost {
    pub fn new() -> Self {
        TerminalHost {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            route_to_session: Mutex::new(HashMap::new()),
            next_session: AtomicU64::new(1),
            output: ByteQueues::new(MAX_QUEUED_BYTES),
        }
    }

    // ---- host side: session model -------------------------------------

    /// Attach `route_id` to a terminal session, creating one if needed.
    ///
    /// * `session_id = Some(id)` and that session exists → **ATTACH**: the
    ///   route joins the session's attachers at `cols`×`rows`, the PTY is
    ///   reconciled to the new per-dimension minimum, and the returned
    ///   [`TermAttach`] carries a scrollback snapshot taken under the same
    ///   guard the live `rx` subscribed under — gapless. `created = false`.
    /// * otherwise → **CREATE**: a fresh PTY+shell for the given id (or a
    ///   minted `term-N` one), this route its first attacher, empty
    ///   scrollback. `created = true`.
    pub fn open(
        &self,
        session_id: Option<&str>,
        route_id: &str,
        cols: u16,
        rows: u16,
    ) -> Result<TermAttach, String> {
        self.open_with(session_id, route_id, cols, rows, default_shell_commands())
    }

    /// [`open`](Self::open) with explicit command candidates (first that
    /// spawns wins) — the test seam, mirroring how the per-OS shell fallbacks
    /// (`$SHELL -l` → `$SHELL` → `/bin/sh`; `pwsh` → `powershell` → `cmd`)
    /// are expressed.
    fn open_with(
        &self,
        session_id: Option<&str>,
        route_id: &str,
        cols: u16,
        rows: u16,
        candidates: Vec<CommandBuilder>,
    ) -> Result<TermAttach, String> {
        // ATTACH: the named session is live → join it.
        if let Some(sid) = session_id {
            let mut sessions = self.sessions.lock();
            if let Some(s) = sessions.get_mut(sid) {
                // Join at the shell's *current* size, not this viewer's 80×24
                // placeholder. The caller passes a placeholder size on attach
                // (the real one arrives moments later via `resize`); folding it
                // into the reconcile straight away would shrink the shared PTY
                // to 80×24 for *everyone* until then — a wrong-width flash on
                // the tabs already attached. Inheriting the current reconciled
                // size leaves the PTY untouched until the real resize lands.
                let (cur_cols, cur_rows) = reconcile_size(&s.attachers);
                s.attachers.insert(
                    route_id.to_string(),
                    Attacher {
                        cols: cur_cols,
                        rows: cur_rows,
                    },
                );
                // Snapshot and subscribe under the scrollback guard so the
                // split is consistent: snapshot = everything broadcast
                // before, rx = everything broadcast after.
                let (scrollback, rx) = {
                    let sb = s.scrollback.lock();
                    (sb.snapshot(), s.out_tx.subscribe())
                };
                // Inheriting the current size, this is usually a no-op — but it
                // keeps the PTY honest and re-broadcasts on the rare change.
                s.reconcile();
                drop(sessions);
                self.route_to_session
                    .lock()
                    .insert(route_id.to_string(), sid.to_string());
                return Ok(TermAttach {
                    session_id: sid.to_string(),
                    scrollback,
                    rx,
                    created: false,
                });
            }
        }

        // CREATE: mint an id if none was given.
        // AMS-06: refuse to spawn past the concurrent-PTY ceiling, so an
        // authorised-but-abusive controller can't fork-bomb this host with
        // shells. Counted here (creation), never on attach.
        {
            let open = self.sessions.lock().len();
            if open >= MAX_LOCAL_SESSIONS {
                return Err(format!(
                    "too many terminal sessions open here ({open}); close one before opening another"
                ));
            }
        }
        let sid = match session_id {
            Some(s) => s.to_string(),
            None => {
                let n = self.next_session.fetch_add(1, Ordering::Relaxed);
                format!("term-{n}")
            }
        };

        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("openpty failed: {e:#}"))?;

        let mut child = None;
        let mut last_err = String::from("no shell candidates");
        for cmd in candidates {
            let label = format!("{:?}", cmd.get_argv());
            match pair.slave.spawn_command(cmd) {
                Ok(c) => {
                    child = Some(c);
                    break;
                }
                Err(e) => {
                    tracing::debug!("terminal shell candidate {label} failed: {e:#}");
                    last_err = format!("{e:#}");
                }
            }
        }
        let mut child = child.ok_or_else(|| format!("couldn't start a shell: {last_err}"))?;
        // The slave end lives on inside the child; holding ours open would
        // keep the reader from ever seeing EOF.
        drop(pair.slave);

        let master = pair.master;
        let mut reader = master
            .try_clone_reader()
            .map_err(|e| format!("pty reader: {e:#}"))?;
        let mut writer = master
            .take_writer()
            .map_err(|e| format!("pty writer: {e:#}"))?;
        let killer = child.clone_killer();
        let mut ctl_killer = child.clone_killer();

        let (out_tx, rx) = tokio::sync::broadcast::channel::<OutMsg>(OUT_QUEUE);
        let (ctl_tx, ctl_rx) = std::sync::mpsc::sync_channel::<CtlMsg>(CTL_QUEUE);
        let scrollback = Arc::new(Mutex::new(Scrollback::new(SCROLLBACK_CAP)));

        // Reader: PTY output → scrollback + broadcast, both under the one
        // scrollback guard. A broadcast send never blocks (a slow attacher
        // lags and drops, it can't wedge the shell), so unlike the old mpsc
        // path this is no longer the flow-control point — the kernel PTY
        // buffer is. Output stays live for everyone.
        let sid_r = sid.clone();
        let reader_tx = out_tx.clone();
        let reader_sb = scrollback.clone();
        spawn_named(&format!("amst-term-read {sid_r}"), move || {
            let mut buf = [0u8; READ_BUF];
            loop {
                match reader.read(&mut buf) {
                    // EOF — on Unix when the shell exits; on Windows often
                    // only once the master drops (the control thread's job).
                    Ok(0) => break,
                    Ok(n) => {
                        let chunk = buf[..n].to_vec();
                        // Append and broadcast under the one scrollback guard
                        // so an attacher snapshotting-then-subscribing can't
                        // slip between the two (no gap, no dup).
                        let mut sb = reader_sb.lock();
                        sb.append(&chunk);
                        let _ = reader_tx.send(OutMsg::Data(chunk));
                    }
                    Err(_) => break,
                }
            }
        });

        // Control: keystrokes + resizes + shutdown. Owns the writer *and
        // the master* — dropping the master on the way out is what
        // unblocks a ConPTY reader that never EOFs.
        let sid_c = sid.clone();
        spawn_named(&format!("amst-term-ctl {sid_c}"), move || {
            let _master = master;
            while let Ok(msg) = ctl_rx.recv() {
                match msg {
                    CtlMsg::Data(bytes) => {
                        use std::io::Write as _;
                        if writer
                            .write_all(&bytes)
                            .and_then(|()| writer.flush())
                            .is_err()
                        {
                            // Writer dead = shell gone; the wait thread
                            // reports it. Stop accepting input.
                            break;
                        }
                    }
                    CtlMsg::Resize { cols, rows } => {
                        let _ = _master.resize(PtySize {
                            rows,
                            cols,
                            pixel_width: 0,
                            pixel_height: 0,
                        });
                    }
                    CtlMsg::Shutdown => {
                        let _ = ctl_killer.kill();
                        break;
                    }
                }
            }
        });

        // Wait: the authoritative end of the session on every OS. The
        // brief linger lets the reader drain the PTY's final bytes so the
        // exit notice lands *after* the shell's goodbye, not racing it; the
        // Exit broadcast goes out under the scrollback guard so it can't
        // overtake a Data chunk still being appended.
        let sid_w = sid.clone();
        let wait_tx = out_tx.clone();
        let wait_sb = scrollback.clone();
        spawn_named(&format!("amst-term-wait {sid_w}"), move || {
            let code = match child.wait() {
                Ok(status) => {
                    if status.signal().is_some() {
                        None
                    } else {
                        Some(status.exit_code() as i32)
                    }
                }
                Err(_) => None,
            };
            std::thread::sleep(std::time::Duration::from_millis(120));
            let _guard = wait_sb.lock();
            let _ = wait_tx.send(OutMsg::Exit(code));
        });

        let mut attachers = HashMap::new();
        attachers.insert(route_id.to_string(), Attacher { cols, rows });
        let session = PtySession {
            ctl_tx,
            killer,
            out_tx,
            scrollback,
            attachers,
            generation: 1,
            title: sid.clone(),
            created_unix: now_unix(),
            last_size: (cols, rows),
        };
        self.sessions.lock().insert(sid.clone(), session);
        self.route_to_session
            .lock()
            .insert(route_id.to_string(), sid.clone());

        Ok(TermAttach {
            session_id: sid,
            scrollback: Vec::new(),
            rx,
            created: true,
        })
    }

    /// Detach a viewer from its session — the opposite of [`open`], and the
    /// graceful default when a viewer window closes or a peer drops. Does
    /// **not** kill the shell: the session lives on for the other attachers,
    /// or for a re-attach, with the screen preserved in scrollback. When the
    /// last attacher leaves, an idle reaper is armed (see
    /// [`SESSION_IDLE_REAP_MS`]).
    pub fn detach(&self, route_id: &str) {
        let sid = self.route_to_session.lock().remove(route_id);
        let Some(sid) = sid else {
            self.output.remove(route_id);
            return;
        };
        let mut now_empty = None;
        {
            let mut sessions = self.sessions.lock();
            if let Some(s) = sessions.get_mut(&sid) {
                s.attachers.remove(route_id);
                if s.attachers.is_empty() {
                    now_empty = Some(s.generation);
                } else {
                    // Lost an attacher → reconcile up to whatever the rest can
                    // show (the min over the survivors), and tell them so they
                    // grow their emulators to match.
                    s.reconcile();
                }
            }
        }
        self.output.remove(route_id);
        if let Some(gen) = now_empty {
            self.arm_idle_reaper(sid, gen);
        }
    }

    /// Whether `route_id` is currently attached to a live session here — the
    /// host pump checks this each tick so a viewer that detached (closed its
    /// tab) stops being streamed to, without killing the shared shell.
    pub fn is_attached(&self, route_id: &str) -> bool {
        self.route_to_session.lock().contains_key(route_id)
    }

    /// Kill the shell for a session id — the explicit "close this terminal"
    /// (as opposed to a viewer merely [`detach`](Self::detach)ing). Removes
    /// the session and every route that mapped to it. Idempotent.
    pub fn close(&self, session_id: &str) {
        let session = self.sessions.lock().remove(session_id);
        if let Some(mut s) = session {
            // Direct kill first — the control thread may be wedged on a
            // write. Shutdown then unblocks/ends it, and dropping ctl_tx
            // closes the channel for good measure.
            let _ = s.killer.kill();
            let _ = s.ctl_tx.try_send(CtlMsg::Shutdown);
        }
        self.route_to_session
            .lock()
            .retain(|_, sid| sid != session_id);
    }

    /// Every live session, for a picker UI.
    pub fn list_sessions(&self) -> Vec<SessionInfo> {
        self.sessions
            .lock()
            .iter()
            .map(|(id, s)| SessionInfo {
                session_id: id.clone(),
                title: s.title.clone(),
                created_unix: s.created_unix,
                attachers: s.attachers.len(),
            })
            .collect()
    }

    /// When a session loses its last attacher, sleep out the grace period and
    /// then kill it — but only if it *still* has no attachers and the
    /// generation we armed against is unchanged (a re-attach, or a recycled
    /// id re-created in the meantime, cancels the reap).
    fn arm_idle_reaper(&self, session_id: String, generation: u64) {
        let sessions = self.sessions.clone();
        crate::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(SESSION_IDLE_REAP_MS)).await;
            let mut map = sessions.lock();
            let still_idle = map
                .get(&session_id)
                .is_some_and(|s| s.attachers.is_empty() && s.generation == generation);
            if still_idle {
                if let Some(mut s) = map.remove(&session_id) {
                    tracing::info!("terminal session {session_id} reaped (idle)");
                    let _ = s.killer.kill();
                    let _ = s.ctl_tx.try_send(CtlMsg::Shutdown);
                }
            }
        });
    }

    // ---- host side: shared input + resize ------------------------------

    /// Feed viewer keystrokes to the hosted PTY. Input from *any* attacher
    /// reaches the one shell. `false` = no such route/session, or its input
    /// queue is full (a flow-stopped shell — bytes dropped rather than
    /// stalling the mesh loop).
    pub fn write(&self, route_id: &str, bytes: Vec<u8>) -> bool {
        self.ctl_send(route_id, CtlMsg::Data(bytes))
    }

    /// Record this route's emulator size and resize the shared PTY to the
    /// **reconciled** size — the minimum cols and minimum rows across all
    /// current attachers, so no attacher's emulator overflows the screen.
    /// `false` = no such route/session, or the ctl queue is full.
    pub fn resize(&self, route_id: &str, cols: u16, rows: u16) -> bool {
        let route_map = self.route_to_session.lock();
        let Some(sid) = route_map.get(route_id) else {
            return false;
        };
        let mut sessions = self.sessions.lock();
        let Some(s) = sessions.get_mut(sid) else {
            return false;
        };
        if let Some(a) = s.attachers.get_mut(route_id) {
            a.cols = cols;
            a.rows = rows;
        } else {
            s.attachers
                .insert(route_id.to_string(), Attacher { cols, rows });
        }
        // Reconcile to the min and, when it changed, broadcast the new shared
        // size so every attacher letterboxes to it.
        if s.reconcile() {
            true
        } else {
            tracing::warn!("terminal {route_id}: resize dropped (shell not draining)");
            false
        }
    }

    fn ctl_send(&self, route_id: &str, msg: CtlMsg) -> bool {
        let route_map = self.route_to_session.lock();
        let Some(sid) = route_map.get(route_id) else {
            return false;
        };
        let sessions = self.sessions.lock();
        let Some(s) = sessions.get(sid) else {
            return false;
        };
        match s.ctl_tx.try_send(msg) {
            Ok(()) => true,
            Err(_) => {
                tracing::warn!("terminal {route_id}: input dropped (shell not draining)");
                false
            }
        }
    }

    // ---- host side: back-compat shims (keep mesh.rs untouched) ---------

    /// Spawn this user's shell in a fresh PTY for `route_id`. Returns the
    /// output stream the caller pumps to the viewer; the session ends with
    /// exactly one [`OutMsg::Exit`] (unless [`stop`](Self::stop) cut it
    /// short, which closes the stream instead).
    ///
    /// A thin bridge over [`open`](Self::open): it adapts the per-session
    /// broadcast (plus the scrollback replay) back to the single
    /// [`tokio::sync::mpsc::Receiver`] the mesh pump expects, one route ⇒ one
    /// session. The session model underneath is the real thing.
    pub fn spawn(&self, route_id: &str) -> Result<tokio::sync::mpsc::Receiver<OutMsg>, String> {
        let attach = self.open(Some(route_id), route_id, 80, 24)?;
        Ok(bridge_to_mpsc(attach))
    }

    /// Tear down whatever this route had here. Preserves today's behaviour:
    /// it **kills** the session this route maps to (the historical `stop`
    /// semantics that callers and tests rely on), then drops the viewer
    /// buffer. Idempotent; safe on either side.
    pub fn stop(&self, route_id: &str) {
        let sid = self.route_to_session.lock().get(route_id).cloned();
        if let Some(sid) = sid {
            self.close(&sid);
        }
        self.output.remove(route_id);
    }

    // ---- viewer side ----------------------------------------------------
    //
    // Thin delegation to the shared [`ByteQueues`]: an output buffer per
    // route exists from route-activation (so the shell's first prompt is
    // never lost), claimed/drained/released by the terminal window.

    pub fn ensure_queue(&self, route_id: &str) {
        self.output.ensure(route_id);
    }

    pub fn watch_output(&self, route_id: &str) -> u64 {
        self.output.watch(route_id)
    }

    pub fn unwatch(&self, route_id: &str, token: u64) {
        self.output.unwatch(route_id, token);
    }

    pub fn poll(&self, route_id: &str) -> Vec<u8> {
        self.output.poll(route_id)
    }

    /// Buffer one inbound output chunk for the watching window. Returns
    /// `true` when the queue went empty → non-empty — the caller's cue to
    /// poke the front-end (mirroring `allmystuff://video-ready`).
    pub fn enqueue(&self, route_id: &str, bytes: Vec<u8>) -> bool {
        self.output.enqueue(route_id, bytes)
    }
}

/// The reconciled PTY size for a set of attachers: the smallest cols and the
/// smallest rows any of them can show, so nobody's emulator overflows. Falls
/// back to 80×24 for an empty set (shouldn't happen — a live session has at
/// least the resizing route — but a sane PTY beats a 0×0 one).
fn reconcile_size(attachers: &HashMap<String, Attacher>) -> (u16, u16) {
    let mut cols = u16::MAX;
    let mut rows = u16::MAX;
    for a in attachers.values() {
        cols = cols.min(a.cols);
        rows = rows.min(a.rows);
    }
    if cols == u16::MAX || rows == u16::MAX {
        (80, 24)
    } else {
        (cols.max(1), rows.max(1))
    }
}

/// Bridge a [`TermAttach`] (broadcast + scrollback) to the single mpsc
/// [`OutMsg`] receiver the mesh pump consumes: replay the scrollback as one
/// `Data`, then forward the live broadcast, treating `Lagged` as "skip ahead"
/// and `Closed` as end-of-session.
fn bridge_to_mpsc(attach: TermAttach) -> tokio::sync::mpsc::Receiver<OutMsg> {
    let TermAttach {
        scrollback, mut rx, ..
    } = attach;
    let (tx, out_rx) = tokio::sync::mpsc::channel::<OutMsg>(OUT_QUEUE);
    crate::spawn(async move {
        if !scrollback.is_empty() && tx.send(OutMsg::Data(scrollback)).await.is_err() {
            return;
        }
        loop {
            match rx.recv().await {
                Ok(msg) => {
                    if tx.send(msg).await.is_err() {
                        break; // pump gone
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    out_rx
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The per-OS shell candidates, best first. Each is a full
/// [`CommandBuilder`] (program + args + env + cwd) ready to spawn.
fn default_shell_commands() -> Vec<CommandBuilder> {
    let mut out = Vec::new();
    #[cfg(windows)]
    {
        // PowerShell when available (pwsh = PowerShell 7+), classic
        // Windows PowerShell next, cmd.exe as the floor.
        for prog in ["pwsh.exe", "powershell.exe"] {
            let mut cmd = CommandBuilder::new(prog);
            cmd.arg("-NoLogo");
            out.push(dressed(cmd));
        }
        out.push(dressed(CommandBuilder::new(
            CommandBuilder::new_default_prog().get_shell(),
        )));
    }
    #[cfg(unix)]
    {
        // The user's own shell ($SHELL, else the password db) as a login
        // shell; the same shell plain for the rare one that rejects `-l`;
        // /bin/sh as the floor.
        let shell = CommandBuilder::new_default_prog().get_shell();
        let mut login = CommandBuilder::new(&shell);
        login.arg("-l");
        out.push(dressed(login));
        out.push(dressed(CommandBuilder::new(&shell)));
        if shell != "/bin/sh" {
            out.push(dressed(CommandBuilder::new("/bin/sh")));
        }
    }
    out
}

/// Home cwd + the terminal identity every spawn gets.
fn dressed(mut cmd: CommandBuilder) -> CommandBuilder {
    if let Some(home) = std::env::var_os("ALLMYSTUFF_USER_HOME")
        .map(std::path::PathBuf::from)
        .or_else(dirs::home_dir)
    {
        cmd.cwd(home);
    }
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd
}

fn spawn_named(name: &str, f: impl FnOnce() + Send + 'static) {
    let _ = std::thread::Builder::new().name(name.to_string()).spawn(f);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// `crate::spawn` (used by the mpsc bridge and the idle reaper) needs a
    /// runtime registered — and `set_runtime` is first-wins, so one
    /// process-wide runtime, kept alive for the whole test binary, is the only
    /// correct shape. Every async-touching test calls this; all but the first
    /// are no-ops that just ensure it's up.
    #[cfg(unix)]
    fn ensure_runtime() {
        use std::sync::OnceLock;
        static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
        let rt = RT.get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap()
        });
        crate::set_runtime(rt.handle().clone());
    }

    /// Collect Data until `pred` matches the transcript (or panic at the
    /// deadline). Returns the transcript so far.
    #[cfg(unix)]
    fn read_until(
        rx: &mut tokio::sync::mpsc::Receiver<OutMsg>,
        pred: impl Fn(&[u8]) -> bool,
        what: &str,
    ) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = Vec::new();
        while Instant::now() < deadline {
            match rx.try_recv() {
                Ok(OutMsg::Data(b)) => {
                    seen.extend_from_slice(&b);
                    if pred(&seen) {
                        return seen;
                    }
                }
                Ok(OutMsg::Resize { .. }) => {}
                Ok(OutMsg::Exit(_)) => break,
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        panic!(
            "didn't see {what} in time; transcript: {:?}",
            String::from_utf8_lossy(&seen)
        );
    }

    /// Collect Data from a broadcast receiver until `pred` matches (or panic).
    #[cfg(unix)]
    fn bcast_read_until(
        rx: &mut tokio::sync::broadcast::Receiver<OutMsg>,
        pred: impl Fn(&[u8]) -> bool,
        what: &str,
    ) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = Vec::new();
        while Instant::now() < deadline {
            match rx.try_recv() {
                Ok(OutMsg::Data(b)) => {
                    seen.extend_from_slice(&b);
                    if pred(&seen) {
                        return seen;
                    }
                }
                Ok(OutMsg::Resize { .. }) => {}
                Ok(OutMsg::Exit(_)) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                Err(_) => break,
            }
        }
        panic!(
            "didn't see {what} in time; transcript: {:?}",
            String::from_utf8_lossy(&seen)
        );
    }

    #[cfg(unix)]
    fn wait_exit(rx: &mut tokio::sync::mpsc::Receiver<OutMsg>) -> Option<i32> {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            match rx.try_recv() {
                Ok(OutMsg::Exit(code)) => return code,
                Ok(_) => {}
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        panic!("no exit in time");
    }

    /// True if the broadcast receiver reports a closed channel within the
    /// window — i.e. the session ended (shell killed/exited).
    #[cfg(unix)]
    fn bcast_closed(rx: &mut tokio::sync::broadcast::Receiver<OutMsg>) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            match rx.try_recv() {
                Ok(OutMsg::Exit(_)) => return true,
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => return true,
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        false
    }

    #[cfg(unix)]
    fn sh(script: &str) -> Vec<CommandBuilder> {
        let mut cmd = CommandBuilder::new("/bin/sh");
        cmd.arg("-c");
        cmd.arg(script);
        vec![cmd]
    }

    #[cfg(unix)]
    #[test]
    fn spawn_echo_roundtrip() {
        ensure_runtime();
        let host = TerminalHost::new();
        let attach = host.open_with(Some("r1"), "r1", 80, 24, sh("cat")).unwrap();
        assert!(attach.created);
        let mut rx = bridge_to_mpsc(attach);
        assert!(host.write("r1", b"hello\n".to_vec()));
        // `cat` echoes back (and the PTY echoes the typing) — either way
        // the bytes round-tripped through a real PTY.
        read_until(
            &mut rx,
            |b| String::from_utf8_lossy(b).contains("hello"),
            "echo",
        );
        host.stop("r1");
        assert!(
            !host.write("r1", b"x".to_vec()),
            "stopped session takes no input"
        );
    }

    #[cfg(unix)]
    #[test]
    fn resize_reaches_the_pty() {
        ensure_runtime();
        let host = TerminalHost::new();
        // The shell waits for a newline, so the resize lands first.
        let attach = host
            .open_with(Some("r2"), "r2", 80, 24, sh("read line; stty size"))
            .unwrap();
        let mut rx = bridge_to_mpsc(attach);
        assert!(host.resize("r2", 100, 40));
        std::thread::sleep(Duration::from_millis(150));
        assert!(host.write("r2", b"\n".to_vec()));
        read_until(
            &mut rx,
            |b| String::from_utf8_lossy(b).contains("40 100"),
            "stty size 40 100",
        );
        host.stop("r2");
    }

    #[cfg(unix)]
    #[test]
    fn exit_code_surfaces() {
        ensure_runtime();
        let host = TerminalHost::new();
        let attach = host
            .open_with(Some("r3"), "r3", 80, 24, sh("exit 3"))
            .unwrap();
        let mut rx = bridge_to_mpsc(attach);
        assert_eq!(wait_exit(&mut rx), Some(3));
    }

    #[cfg(unix)]
    #[test]
    fn stop_kills_promptly() {
        ensure_runtime();
        let host = TerminalHost::new();
        let attach = host
            .open_with(Some("r4"), "r4", 80, 24, sh("sleep 30"))
            .unwrap();
        let mut rx = bridge_to_mpsc(attach);
        std::thread::sleep(Duration::from_millis(150));
        host.stop("r4");
        // Killed → the wait-thread reports a signal death (no code) quickly,
        // not after 30s, and then the broadcast closes. The bridge forwards
        // the Exit if it raced in before the close.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut ended = false;
        while Instant::now() < deadline {
            match rx.try_recv() {
                Ok(OutMsg::Exit(_)) => {
                    ended = true;
                    break;
                }
                Ok(_) => {}
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    ended = true;
                    break;
                }
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        assert!(ended, "stop ends the session promptly");
    }

    // ---- new session-model tests --------------------------------------

    #[cfg(unix)]
    #[test]
    fn multi_attach_fan_out() {
        ensure_runtime();
        let host = TerminalHost::new();
        // First attacher creates the session.
        let a = host
            .open_with(
                Some("sess1"),
                "rA",
                80,
                24,
                sh("echo fan-out-marker; sleep 5"),
            )
            .unwrap();
        assert!(a.created);
        // Second attaches to the same id.
        let b = host
            .open_with(Some("sess1"), "rB", 80, 24, sh("unused"))
            .unwrap();
        assert!(!b.created, "second open ATTACHES");
        let mut rxa = a.rx;
        let mut rxb = b.rx;
        bcast_read_until(
            &mut rxa,
            |x| String::from_utf8_lossy(x).contains("fan-out-marker"),
            "marker on A",
        );
        // B may rely on scrollback for output produced before it subscribed.
        let mut seen_b = b.scrollback.clone();
        if !String::from_utf8_lossy(&seen_b).contains("fan-out-marker") {
            let live = bcast_read_until(
                &mut rxb,
                |x| String::from_utf8_lossy(x).contains("fan-out-marker"),
                "marker on B (live)",
            );
            seen_b.extend_from_slice(&live);
        }
        assert!(String::from_utf8_lossy(&seen_b).contains("fan-out-marker"));
        host.close("sess1");
    }

    #[cfg(unix)]
    #[test]
    fn scrollback_replay() {
        ensure_runtime();
        let host = TerminalHost::new();
        let a = host
            .open_with(
                Some("sb1"),
                "rA",
                80,
                24,
                sh("echo scrollback-token; sleep 5"),
            )
            .unwrap();
        let mut rxa = a.rx;
        // Make sure the output has been produced (and thus appended to
        // scrollback) before the second attach.
        bcast_read_until(
            &mut rxa,
            |x| String::from_utf8_lossy(x).contains("scrollback-token"),
            "token on A",
        );
        let b = host
            .open_with(Some("sb1"), "rB", 80, 24, sh("unused"))
            .unwrap();
        assert!(!b.created);
        assert!(
            String::from_utf8_lossy(&b.scrollback).contains("scrollback-token"),
            "fresh attach replays scrollback: {:?}",
            String::from_utf8_lossy(&b.scrollback)
        );
        host.close("sb1");
    }

    #[cfg(unix)]
    #[test]
    fn shared_input_reaches_one_shell() {
        ensure_runtime();
        let host = TerminalHost::new();
        let a = host
            .open_with(Some("si1"), "rA", 80, 24, sh("cat"))
            .unwrap();
        let b = host
            .open_with(Some("si1"), "rB", 80, 24, sh("unused"))
            .unwrap();
        let mut rxa = a.rx;
        let mut rxb = b.rx;
        // Input on B's route reaches the one shell; cat echoes it, both see it.
        assert!(host.write("rB", b"from-B\n".to_vec()));
        bcast_read_until(
            &mut rxa,
            |x| String::from_utf8_lossy(x).contains("from-B"),
            "B's input on A",
        );
        bcast_read_until(
            &mut rxb,
            |x| String::from_utf8_lossy(x).contains("from-B"),
            "B's input on B",
        );
        // And input on A's route likewise.
        assert!(host.write("rA", b"from-A\n".to_vec()));
        bcast_read_until(
            &mut rxb,
            |x| String::from_utf8_lossy(x).contains("from-A"),
            "A's input on B",
        );
        host.close("si1");
    }

    #[cfg(unix)]
    #[test]
    fn detach_does_not_kill() {
        ensure_runtime();
        let host = TerminalHost::new();
        let a = host
            .open_with(Some("dt1"), "rA", 80, 24, sh("cat"))
            .unwrap();
        let b = host
            .open_with(Some("dt1"), "rB", 80, 24, sh("unused"))
            .unwrap();
        let mut rxb = b.rx;
        drop(a.rx);
        // Detaching A leaves the shell alive for B.
        host.detach("rA");
        assert!(!host.write("rA", b"x".to_vec()), "detached route is gone");
        assert_eq!(
            host.list_sessions().len(),
            1,
            "session still live after detach"
        );
        // B still drives and receives.
        assert!(host.write("rB", b"still-here\n".to_vec()));
        bcast_read_until(
            &mut rxb,
            |x| String::from_utf8_lossy(x).contains("still-here"),
            "B still receiving after A detached",
        );
        // Closing (the explicit end) tears it down.
        host.close("dt1");
        assert!(bcast_closed(&mut rxb), "close ends the session");
        assert!(host.list_sessions().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn resize_reconciles_to_min() {
        ensure_runtime();
        let host = TerminalHost::new();
        // A creates the shell at 100x40. B *attaches* — it joins at the
        // shell's current size (no shrink: production passes a placeholder
        // here, the real size arrives via `resize`), then B's real 80x50
        // reconciles the shared PTY to 80 cols, 40 rows (the min each shows).
        let _a = host
            .open_with(Some("rz1"), "rA", 100, 40, sh("read line; stty size"))
            .unwrap();
        let b = host
            .open_with(Some("rz1"), "rB", 80, 24, sh("unused"))
            .unwrap();
        let mut rxb = b.rx;
        assert!(host.resize("rB", 80, 50), "B's real size arrives");
        std::thread::sleep(Duration::from_millis(150));
        // Newline releases the `read`, then stty prints the reconciled size.
        assert!(host.write("rB", b"\n".to_vec()));
        bcast_read_until(
            &mut rxb,
            |x| String::from_utf8_lossy(x).contains("40 80"),
            "reconciled stty size 40 80",
        );
        host.close("rz1");
    }

    #[cfg(unix)]
    #[test]
    fn attach_does_not_shrink_to_the_joiners_placeholder() {
        ensure_runtime();
        let host = TerminalHost::new();
        // A creates the shell at 100x40.
        let a = host
            .open_with(Some("sz"), "rA", 100, 40, sh("read line; stty size"))
            .unwrap();
        let mut rxa = a.rx;
        // B joins with the 80x24 placeholder the host always passes on attach
        // (B's real size arrives later via `resize`). The shared PTY must stay
        // 100x40 — a placeholder attach must not shrink it for A.
        let _b = host
            .open_with(Some("sz"), "rB", 80, 24, sh("unused"))
            .unwrap();
        std::thread::sleep(Duration::from_millis(150));
        assert!(host.write("rA", b"\n".to_vec()));
        bcast_read_until(
            &mut rxa,
            |x| String::from_utf8_lossy(x).contains("40 100"),
            "PTY stays 100x40 after a placeholder attach (no shrink)",
        );
        host.close("sz");
    }

    #[cfg(unix)]
    #[test]
    fn double_open_reattaches() {
        ensure_runtime();
        let host = TerminalHost::new();
        let first = host
            .open_with(Some("dup"), "rA", 80, 24, sh("sleep 5"))
            .unwrap();
        assert!(first.created, "first open creates");
        let second = host
            .open_with(Some("dup"), "rB", 80, 24, sh("unused"))
            .unwrap();
        assert!(!second.created, "second open on the same id ATTACHES");
        assert_eq!(second.session_id, "dup");
        host.close("dup");
    }

    /// How many CSI 6 n (cursor-position) probes the transcript holds —
    /// what a ConPTY-backed shell sends its terminal before painting.
    fn dsr_probes(hay: &[u8]) -> usize {
        const QUERY: &[u8] = b"\x1b[6n";
        hay.windows(QUERY.len()).filter(|w| *w == QUERY).count()
    }

    #[test]
    fn dsr_probe_counting_finds_each_query() {
        assert_eq!(dsr_probes(b""), 0);
        assert_eq!(dsr_probes(b"plain output"), 0);
        assert_eq!(dsr_probes(b"\x1b[6n"), 1);
        assert_eq!(dsr_probes(b"a\x1b[6nb\x1b[6nc"), 2);
        // A truncated probe (chunk boundary) doesn't count until the rest
        // arrives — the caller re-counts over the whole transcript.
        assert_eq!(dsr_probes(b"\x1b["), 0);
    }

    #[test]
    fn scrollback_ring_caps_and_keeps_the_tail() {
        let mut sb = Scrollback::new(8);
        sb.append(b"abc");
        assert_eq!(sb.snapshot(), b"abc");
        sb.append(b"defghij"); // total 10 > cap 8 → oldest drop
        assert_eq!(sb.snapshot(), b"cdefghij");
        // A single chunk bigger than the cap keeps only its tail.
        sb.append(b"0123456789");
        assert_eq!(sb.snapshot(), b"23456789");
    }

    #[cfg(windows)]
    #[test]
    fn spawn_cmd_echo_and_exit() {
        let mut cmd = CommandBuilder::new("cmd.exe");
        cmd.arg("/C");
        cmd.arg("echo hello-from-conpty");
        let host = TerminalHost::new();
        let attach = host.open_with(Some("rw"), "rw", 80, 24, vec![cmd]).unwrap();
        let mut rx = bridge_to_mpsc(attach);

        // ConPTY probes its "terminal" with CSI 6 n (report cursor
        // position) and holds output until something answers — in the
        // real app xterm.js answers and the reply rides the route back.
        // The test plays the emulator: answer every probe it sees.
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut seen: Vec<u8> = Vec::new();
        let mut answered = 0usize;
        let mut exit: Option<Option<i32>> = None;
        while Instant::now() < deadline {
            match rx.try_recv() {
                Ok(OutMsg::Data(b)) => {
                    seen.extend_from_slice(&b);
                    while answered < dsr_probes(&seen) {
                        assert!(host.write("rw", b"\x1b[1;1R".to_vec()));
                        answered += 1;
                    }
                }
                Ok(OutMsg::Exit(code)) => {
                    exit = Some(code);
                    break;
                }
                Ok(OutMsg::Resize { .. }) => {}
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        // The exit linger orders Data before Exit, but drain any tail
        // that was already queued before asserting on the transcript.
        while let Ok(OutMsg::Data(b)) = rx.try_recv() {
            seen.extend_from_slice(&b);
        }
        let transcript = String::from_utf8_lossy(&seen);
        assert!(
            transcript.contains("hello-from-conpty"),
            "transcript: {transcript:?}"
        );
        assert_eq!(exit, Some(Some(0)));
    }

    // The watcher-queue behaviours (framing, eager-queue adoption, token
    // scoping, overflow) are tested where they now live: `byte_queues`.
}
