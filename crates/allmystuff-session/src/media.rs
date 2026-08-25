//! The video + input frames carried over active display/input routes —
//! the rest of the media plane next to [`crate::AudioFrame`].
//!
//! The transport is deliberately the "basic known stuff": a display route
//! is an **MJPEG stream** (piKVM's default — each frame a standalone JPEG,
//! so loss costs one frame and seeking/decoding state never exists), and an
//! input route is a stream of small **normalized HID events** (mouse moves
//! in 0..1 so the two ends never negotiate resolutions; keys as the DOM
//! `KeyboardEvent.key` value so layouts resolve on the side that typed).
//!
//! All three frame kinds share the daemon's one media channel. Audio keeps
//! its original untagged shape (a v0.1.0 peer still decodes it); video and
//! input are tagged `"t":"video"` / `"t":"input"`, and [`MediaPayload`]
//! demuxes by that tag — absent tag means audio.

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::AudioFrame;

/// One JPEG-encoded frame of a display route's stream — or one *chunk* of
/// it: the mesh's data channel caps a message at ~64 KiB (the WebRTC SCTP
/// transport's maximum message size), and a desktop frame routinely beats
/// that, so a large frame travels as several messages sharing a `seq`,
/// reassembled by [`VideoAssembler`] at the sink. Losing any chunk just
/// loses that frame — the next one stands alone, MJPEG's whole virtue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoFrame {
    /// Tag for demuxing off the shared media channel. Always `"video"`.
    pub t: MediaTagVideo,
    /// The route this frame belongs to.
    pub route: String,
    pub seq: u64,
    /// Encoded frame dimensions (after any sender-side downscale).
    pub width: u32,
    pub height: u32,
    /// The capture source's full resolution — what normalized input
    /// coordinates map back onto.
    pub source_width: u32,
    pub source_height: u32,
    /// This piece's index within `chunks`. Defaults (0 of 1) mean "the
    /// whole frame in one message" — the common case for small frames.
    #[serde(default)]
    pub chunk: u16,
    #[serde(default = "one_chunk")]
    pub chunks: u16,
    /// The JPEG bytes (of this chunk), base64 on the wire (the daemon
    /// channel is JSON).
    #[serde(with = "bytes_b64")]
    pub jpeg: Vec<u8>,
}

fn one_chunk() -> u16 {
    1
}

/// One keyboard / mouse event of an input route's stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputEvent {
    /// Tag for demuxing off the shared media channel. Always `"input"`.
    pub t: MediaTagInput,
    pub route: String,
    pub seq: u64,
    #[serde(flatten)]
    pub action: InputAction,
}

/// What the far keyboard/mouse did. Coordinates are normalized 0..1 over
/// the *source screen* of the paired display route, so neither end needs
/// the other's resolution; the injecting side multiplies by its own.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputAction {
    MouseMove {
        x: f64,
        y: f64,
        /// Which of the remote's monitors the coordinates are normalized
        /// over — the `screen:<id>` capability's id, so control follows
        /// the screen the console is showing. `None` = the primary (and
        /// what an older sender's events decode to).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        screen: Option<u32>,
    },
    /// `button`: 0 left, 1 middle, 2 right (the DOM convention).
    MouseButton { button: u8, down: bool },
    /// Scroll in wheel lines (positive = down / right).
    Wheel { dx: f64, dy: f64 },
    /// `key` is the DOM `KeyboardEvent.key` value — a printable character
    /// ("a", "?") or a named key ("Enter", "ArrowLeft", "Shift").
    ///
    /// `code` is the DOM `KeyboardEvent.code` value — the *physical* key
    /// ("KeyC", "Digit1"), which modifiers never change. Key combinations
    /// need it twice over: a chord like Ctrl+C must land on the remote's
    /// *C key*, not on whatever character the sender's layout composed
    /// under the held modifiers, and the keyup of a chord must release
    /// the same key its keydown pressed even when the modifier lifted
    /// in between (Shift+1 goes down as "!" but comes up as "1" — only
    /// the code ties the pair together). Absent from an older sender;
    /// absent on the wire when unknown, so an older receiver sees the
    /// exact shape it always did.
    Key {
        key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
        down: bool,
    },
    /// Relative mouse motion in device pixels — the viewer's Pointer Lock
    /// deltas (game-mode aiming: fullscreen mouse capture). No
    /// normalization and no screen clamp: FPS camera control needs the
    /// raw deltas, not a position. An older receiver ignores it via the
    /// catch-all below (motion just doesn't land there — the viewer's
    /// pointer-lock UI is the newer build's feature to offer).
    MouseMoveRel { dx: f64, dy: f64 },
    /// An input kind a newer build introduced. Decodes here and is ignored
    /// rather than failing the whole frame, so a future action can never
    /// drop the keyboard/mouse events this build does understand.
    #[serde(other)]
    Unknown,
}

/// One event of a terminal route's stream — the byte-level conversation
/// between an xterm.js viewer and the PTY a host spawned for it. Bytes are
/// opaque to the wire (the emulator and the PTY speak VT between
/// themselves); the frame just carries them, plus the two control events a
/// session needs: the viewer resizing its emulator, and the host reporting
/// the shell's end.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TermFrame {
    /// Tag for demuxing off the shared media channel. Always `"term"`.
    pub t: MediaTagTerm,
    pub route: String,
    pub seq: u64,
    #[serde(flatten)]
    pub event: TermEvent,
}

/// What happened on the terminal route. `Data` flows both ways (keystrokes
/// up, PTY output down); `Resize` only viewer → host; `Exit` only host →
/// viewer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TermEvent {
    /// Raw PTY bytes, base64 on the wire (the daemon channel is JSON).
    Data {
        #[serde(with = "bytes_b64")]
        bytes: Vec<u8>,
    },
    /// The viewer's emulator was resized; the host resizes the PTY so the
    /// shell relays out at the right dimensions.
    Resize { cols: u16, rows: u16 },
    /// The shell ended. `None` = killed / no status to report.
    Exit {
        #[serde(default)]
        code: Option<i32>,
    },
    /// A terminal event a newer build introduced. Ignored rather than
    /// failing the whole frame.
    #[serde(other)]
    Unknown,
}

impl TermFrame {
    pub fn new(route: impl Into<String>, seq: u64, event: TermEvent) -> Self {
        TermFrame {
            t: MediaTagTerm::Term,
            route: route.into(),
            seq,
            event,
        }
    }

    /// Split `bytes` into channel-sized [`TermEvent::Data`] frames — each
    /// carries at most `max_bytes` so the full JSON message (base64 +
    /// envelope) stays under the transport's ceiling. Sequence numbers
    /// increment per piece starting at `first_seq`; an empty payload still
    /// yields one (empty) frame so a write is never silently dropped.
    pub fn data_frames(
        route: &str,
        first_seq: u64,
        bytes: &[u8],
        max_bytes: usize,
    ) -> Vec<TermFrame> {
        let max = max_bytes.max(1);
        if bytes.len() <= max {
            return vec![TermFrame::new(
                route,
                first_seq,
                TermEvent::Data {
                    bytes: bytes.to_vec(),
                },
            )];
        }
        bytes
            .chunks(max)
            .enumerate()
            .map(|(i, piece)| {
                TermFrame::new(
                    route,
                    first_seq + i as u64,
                    TermEvent::Data {
                        bytes: piece.to_vec(),
                    },
                )
            })
            .collect()
    }
}

/// One message of a files route's stream — the request/response
/// conversation between a file-manager viewer and the host whose disk it
/// browses. Requests flow viewer → host, responses host → viewer; every
/// event carries the viewer-minted request id (`req`) it belongs to, so
/// concurrent operations (a directory listing while a download streams)
/// never tangle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileFrame {
    /// Tag for demuxing off the shared media channel. Always `"file"`.
    pub t: MediaTagFile,
    pub route: String,
    pub seq: u64,
    #[serde(flatten)]
    pub event: FileEvent,
}

/// One directory entry of a [`FileEvent::Entries`] listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub name: String,
    /// Filesystem-native identity, opaque outside the source device. Combined
    /// with the source device id it survives rename/move where the host OS can
    /// provide a durable file id. Older peers omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_id: Option<String>,
    /// Native hidden/system state. Display policy remains viewer-local.
    #[serde(default)]
    pub hidden: bool,
    /// `true` for a directory (what the listing navigates into).
    pub dir: bool,
    /// Byte size (0 for directories).
    #[serde(default)]
    pub size: u64,
    /// Last-modified time, seconds since the Unix epoch. `None` when the
    /// filesystem couldn't say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<u64>,
    /// `true` when the entry is a symlink (shown, never followed for ops).
    #[serde(default)]
    pub symlink: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileVolume {
    pub name: String,
    pub path: String,
    pub size: u64,
    #[serde(default)]
    pub removable: bool,
}

/// What happened on the files route. The viewer asks (`List`, `Read`,
/// `Write`, `Mkdir`, `Rename`, `Delete`); the host answers (`Entries`,
/// `Chunk`, `Ok`, `Err`). Paths are the host's own (absolute once the
/// first `Entries` reply tells the viewer where home is; `""` or `"~"`
/// mean the host's home directory).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FileEvent {
    /// Query filesystem usage for a scoped native-drive route. Answered with
    /// `QuotaInfo` or `Err`.
    Quota {
        req: u64,
    },
    /// List mounted filesystem roots. Used by the KVM virtual-media picker so
    /// a remote removable disk is reachable without guessing its drive letter.
    Volumes {
        req: u64,
    },
    /// List a directory. Answered with `Entries` or `Err`.
    List {
        req: u64,
        path: String,
        /// Opaque continuation minted by the host. Missing keeps the legacy
        /// one-shot behavior for older viewers; upgraded viewers always send
        /// a bounded limit.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cursor: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u16>,
    },
    /// Read a whole file. Answered with a stream of `Chunk`s (the last
    /// has `eof: true`) or `Err`.
    Read {
        req: u64,
        path: String,
    },
    /// Read file metadata without transferring its contents.
    Stat {
        req: u64,
        path: String,
    },
    /// Read at most `len` bytes beginning at `offset`. Native filesystem
    /// bridges use this for random-access reads.
    ReadRange {
        req: u64,
        path: String,
        offset: u64,
        len: u64,
    },
    /// Fetch a file offered into a room's **Shared Files** area, named by
    /// the opaque `token` the uploader minted (never a path — a `:shared`
    /// route can't browse the disk, only pull what was explicitly shared).
    /// The host resolves the token to a path, checks the requester is a
    /// member the file was shared with, and answers with the same `Chunk`
    /// stream a `Read` would — or `Err` when the token is unknown or not
    /// shared with this peer.
    Fetch {
        req: u64,
        token: String,
    },
    /// Write one piece of a file. The first piece (`append: false`)
    /// creates/truncates; later pieces append. The host answers `Ok` once
    /// the piece with `eof: true` is on disk (or `Err` at any point).
    Write {
        req: u64,
        path: String,
        #[serde(with = "bytes_b64")]
        data: Vec<u8>,
        #[serde(default)]
        append: bool,
        #[serde(default)]
        eof: bool,
    },
    /// Write one random-access range. `truncate` sets the file length to
    /// `offset` before the bytes are written (used by create/truncate).
    WriteRange {
        req: u64,
        path: String,
        offset: u64,
        #[serde(with = "bytes_b64")]
        data: Vec<u8>,
        #[serde(default)]
        truncate: bool,
    },
    /// Create a directory (parents included). Answered with `Ok`/`Err`.
    Mkdir {
        req: u64,
        path: String,
    },
    /// Rename/move within the host. Answered with `Ok`/`Err`.
    Rename {
        req: u64,
        from: String,
        to: String,
    },
    /// Delete a file or directory (recursively). Answered with `Ok`/`Err`.
    Delete {
        req: u64,
        path: String,
    },
    /// A directory listing. `home` is the host's home directory — sent on
    /// every listing so the viewer can mark it and start there.
    Entries {
        req: u64,
        path: String,
        home: String,
        entries: Vec<FileEntry>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        next_cursor: Option<String>,
    },
    VolumeList {
        req: u64,
        volumes: Vec<FileVolume>,
    },
    /// Filesystem usage returned by `Quota`.
    QuotaInfo {
        req: u64,
        used: u64,
        total: u64,
    },
    /// Metadata returned by `Stat`.
    Metadata {
        req: u64,
        entry: FileEntry,
    },
    /// One piece of a `Read`. `total` is the file's full size (so the
    /// viewer can show progress); `eof` marks the last piece.
    Chunk {
        req: u64,
        #[serde(with = "bytes_b64")]
        data: Vec<u8>,
        total: u64,
        #[serde(default)]
        eof: bool,
    },
    /// The request succeeded (`Write`/`Mkdir`/`Rename`/`Delete`).
    Ok {
        req: u64,
    },
    /// The request failed, with the host's reason.
    Err {
        req: u64,
        reason: String,
    },
    /// A files-plane event a newer build introduced. Ignored rather than
    /// failing the whole frame.
    #[serde(other)]
    Unknown,
}

impl FileEvent {
    /// The request id this event belongs to, whatever its kind. An
    /// unrecognised (newer-build) event belongs to no request we track, so
    /// it reads as `0` — it is dropped before this is consulted anyway.
    pub fn req(&self) -> u64 {
        match self {
            FileEvent::Quota { req }
            | FileEvent::Volumes { req }
            | FileEvent::List { req, .. }
            | FileEvent::Read { req, .. }
            | FileEvent::Stat { req, .. }
            | FileEvent::ReadRange { req, .. }
            | FileEvent::Fetch { req, .. }
            | FileEvent::Write { req, .. }
            | FileEvent::WriteRange { req, .. }
            | FileEvent::Mkdir { req, .. }
            | FileEvent::Rename { req, .. }
            | FileEvent::Delete { req, .. }
            | FileEvent::Entries { req, .. }
            | FileEvent::VolumeList { req, .. }
            | FileEvent::QuotaInfo { req, .. }
            | FileEvent::Metadata { req, .. }
            | FileEvent::Chunk { req, .. }
            | FileEvent::Ok { req }
            | FileEvent::Err { req, .. } => *req,
            FileEvent::Unknown => 0,
        }
    }
}

impl FileFrame {
    pub fn new(route: impl Into<String>, seq: u64, event: FileEvent) -> Self {
        FileFrame {
            t: MediaTagFile::File,
            route: route.into(),
            seq,
            event,
        }
    }
}

/// One clipboard transfer over a clipboard route. Content only ever crosses
/// the wire at an explicit moment — never on a bare copy — so each machine
/// keeps its own clipboard and neither silently clobbers the other:
///   * on **paste**, the controller pushes its clipboard so the far side
///     pastes *that* (the keystroke rides the paired control route right
///     behind this frame, on the same ordered channel, and lands it), and
///   * on **copy/cut from the remote**, the controller first forwards the
///     copy keystroke, then sends a [`Pull`](ClipboardEvent::Pull) and the
///     remote replies with its freshly-copied clipboard down this same route.
///
/// Either way the receiving machine writes the payload to its OS clipboard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardFrame {
    /// Tag for demuxing off the shared media channel. Always `"clip"`.
    pub t: MediaTagClipboard,
    pub route: String,
    pub seq: u64,
    #[serde(flatten)]
    pub event: ClipboardEvent,
}

/// What crossed on a clipboard route. `Text` is one frame; an image or a
/// set of files is a *transfer* — an [`Open`](ClipboardEvent::Open)
/// manifest, then [`Chunk`](ClipboardEvent::Chunk)s of base64 bytes (the
/// daemon channel caps a message near 64 KiB, so anything bigger is split
/// exactly like a video frame), then a [`Close`](ClipboardEvent::Close)
/// that commits it to the far clipboard. File bytes stream from disk and
/// land in a staging dir on the receiver, whose OS clipboard is then
/// pointed at them — so a paste in a file manager materializes them. The
/// bytes ride this route in pieces; the *content* of those pieces is
/// written with the same logic the files plane already uses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClipboardEvent {
    /// Plain UTF-8 text — the everyday copy/paste, one frame.
    Text { text: String },
    /// Begin a binary transfer. `transfer` scopes its chunks; `content`
    /// says whether the items are an image or files; `items` names each
    /// piece and its byte length (one item for an image, one per file).
    Open {
        transfer: u64,
        content: ClipboardContentKind,
        items: Vec<ClipboardItem>,
    },
    /// One ≤[`CLIPBOARD_CHUNK_BYTES`] piece of `item`'s bytes, base64 on the
    /// wire. An item's pieces arrive in order, back to back.
    Chunk {
        transfer: u64,
        item: u32,
        #[serde(with = "bytes_b64")]
        data: Vec<u8>,
    },
    /// Every chunk delivered — assemble and write to the OS clipboard.
    Close { transfer: u64 },
    /// Receiver -> sender confirmation for a binary transfer. Sent only by
    /// peers advertising `clipboard-receipts`; `error = None` means the far
    /// OS accepted the image/native file list, while `Some` explains why it
    /// did not. This lets a console wait to inject paste until the destination
    /// clipboard is genuinely ready.
    Applied {
        transfer: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Controller → controlled: read your clipboard *now* and send it back
    /// down this same route. The mirror of the everyday paste flow, for when
    /// someone driving the console copies or cuts **from** the remote
    /// (Ctrl/Cmd+C·X): the keystroke rides the control route first so the
    /// remote copies its selection into its own clipboard, then this asks for
    /// that content so it lands on the controller's clipboard. The reply rides
    /// this route in reverse — text in one [`Text`](ClipboardEvent::Text)
    /// frame, an image or files as an [`Open`](ClipboardEvent::Open) /
    /// [`Chunk`](ClipboardEvent::Chunk) / [`Close`](ClipboardEvent::Close)
    /// transfer — exactly like a paste going the other way. Carries no fields:
    /// it names no transfer of its own; the reply mints that.
    Pull,
    /// A clipboard event a newer build introduced. Ignored rather than
    /// failing the whole frame.
    #[serde(other)]
    Unknown,
}

/// Whether a clipboard transfer carries an image or a set of files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardContentKind {
    Image,
    Files,
    /// A content kind a newer build introduced. The transfer is ignored
    /// rather than failing the [`ClipboardEvent::Open`] decode.
    Unknown,
}

// Lenient decode: a content kind this build doesn't recognise reads as
// `Unknown` instead of erroring (`#[serde(other)]` isn't available on a
// string-valued enum), so a future clipboard kind can't fail the `Open`.
impl<'de> Deserialize<'de> for ClipboardContentKind {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(match String::deserialize(d)?.as_str() {
            "image" => ClipboardContentKind::Image,
            "files" => ClipboardContentKind::Files,
            _ => ClipboardContentKind::Unknown,
        })
    }
}

/// One piece of a clipboard transfer: a file's base name (or `image.png`
/// for an image), and its total byte length so the receiver can pre-size
/// and cap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardItem {
    pub name: String,
    pub size: u64,
}

/// Max bytes per [`ClipboardEvent::Chunk`] before base64 — kept well under
/// the daemon channel's ~64 KiB message ceiling once base64 (×4/3) and the
/// JSON envelope are added.
pub const CLIPBOARD_CHUNK_BYTES: usize = 32 * 1024;

impl ClipboardFrame {
    pub fn new(route: impl Into<String>, seq: u64, event: ClipboardEvent) -> Self {
        ClipboardFrame {
            t: MediaTagClipboard::Clipboard,
            route: route.into(),
            seq,
            event,
        }
    }
}

/// One frame of a **site** route — AllMyStuff's reverse proxy. A site route
/// carries a raw TCP service (a local web app, a database) across the mesh:
/// the *client* side accepts local TCP connections on a mapped port and
/// tunnels each one to the *host*, which connects it to `127.0.0.1:<port>`
/// and pumps bytes back. One route multiplexes many connections — a browser
/// opens several at once — so every frame names its `conn` (a client-minted
/// id scoping that connection within the route), exactly as a [`FileFrame`]
/// names its `req`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteFrame {
    /// Tag for demuxing off the shared media channel. Always `"site"`.
    pub t: MediaTagSite,
    pub route: String,
    pub seq: u64,
    #[serde(flatten)]
    pub event: SiteEvent,
}

/// What happened on one tunneled connection of a site route. The client
/// opens (`Open`), both ends stream (`Data`), and either end closes
/// (`Close`). The host validates `Open`'s `port` against the site it
/// advertised before connecting — the advert is the allow-list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SiteEvent {
    /// Client → host: open a tunnel to the site's port. `conn` scopes this
    /// connection's frames within the route; `port` is the advertised site
    /// port the host must re-validate before dialing loopback.
    Open { conn: u64, port: u16 },
    /// Either direction: one chunk of `conn`'s byte stream (base64 on the
    /// wire — the channel is JSON, like every media frame). Capped well
    /// under the daemon's ~64 KiB message ceiling by the sender.
    Data {
        conn: u64,
        #[serde(with = "bytes_b64")]
        data: Vec<u8>,
    },
    /// Either direction: `conn`'s stream ended (EOF, reset, or teardown).
    /// The other side closes its half of the local socket.
    Close { conn: u64 },
    /// A site event a newer build introduced. Ignored rather than failing
    /// the whole frame.
    #[serde(other)]
    Unknown,
}

impl SiteEvent {
    /// The connection id this event belongs to, whatever its kind. An
    /// unrecognised (newer-build) event scopes to no connection we track, so
    /// it reads as `0` — it is dropped before this is consulted anyway.
    pub fn conn(&self) -> u64 {
        match self {
            SiteEvent::Open { conn, .. }
            | SiteEvent::Data { conn, .. }
            | SiteEvent::Close { conn } => *conn,
            SiteEvent::Unknown => 0,
        }
    }
}

impl SiteFrame {
    pub fn new(route: impl Into<String>, seq: u64, event: SiteEvent) -> Self {
        SiteFrame {
            t: MediaTagSite::Site,
            route: route.into(),
            seq,
            event,
        }
    }
}

/// Max raw bytes per [`SiteEvent::Data`] frame before base64 — kept well
/// under the daemon channel's ~64 KiB message ceiling once base64 (×4/3)
/// and the JSON envelope are added, the same budget the files plane uses.
pub const SITE_CHUNK_BYTES: usize = 40 * 1024;

/// Everything that can arrive on the media channel, demuxed by the `t`
/// tag (no tag = audio, the original frame shape).
#[derive(Debug, Clone, PartialEq)]
pub enum MediaPayload {
    Audio(AudioFrame),
    Video(VideoFrame),
    VideoStatus(VideoStatusFrame),
    Input(InputEvent),
    Terminal(TermFrame),
    File(FileFrame),
    Clipboard(ClipboardFrame),
    Site(SiteFrame),
}

impl MediaPayload {
    /// Decode a media-channel payload. `None` for frames we don't
    /// understand (e.g. a newer peer's new kind) — drop, never error.
    pub fn decode(payload: serde_json::Value) -> Option<MediaPayload> {
        match payload.get("t").and_then(|t| t.as_str()) {
            Some("video") => serde_json::from_value(payload)
                .ok()
                .map(MediaPayload::Video),
            Some("vstat") => serde_json::from_value(payload)
                .ok()
                .map(MediaPayload::VideoStatus),
            Some("input") => serde_json::from_value(payload)
                .ok()
                .map(MediaPayload::Input),
            Some("term") => serde_json::from_value(payload)
                .ok()
                .map(MediaPayload::Terminal),
            Some("file") => serde_json::from_value(payload).ok().map(MediaPayload::File),
            Some("clip") => serde_json::from_value(payload)
                .ok()
                .map(MediaPayload::Clipboard),
            Some("site") => serde_json::from_value(payload).ok().map(MediaPayload::Site),
            Some(_) => None,
            None => serde_json::from_value(payload)
                .ok()
                .map(MediaPayload::Audio),
        }
    }

    /// The route id the frame belongs to, whatever its kind.
    pub fn route(&self) -> &str {
        match self {
            MediaPayload::Audio(f) => &f.route,
            MediaPayload::Video(f) => &f.route,
            MediaPayload::VideoStatus(f) => &f.route,
            MediaPayload::Input(f) => &f.route,
            MediaPayload::Terminal(f) => &f.route,
            MediaPayload::File(f) => &f.route,
            MediaPayload::Clipboard(f) => &f.route,
            MediaPayload::Site(f) => &f.route,
        }
    }
}

// Unit-variant tags so the structs serialize with a literal `"t":"…"`
// field (and refuse to deserialize anything else), without a hand-written
// serde impl.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum MediaTagVideo {
    #[default]
    #[serde(rename = "video")]
    Video,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum MediaTagInput {
    #[default]
    #[serde(rename = "input")]
    Input,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum MediaTagTerm {
    #[default]
    #[serde(rename = "term")]
    Term,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum MediaTagFile {
    #[default]
    #[serde(rename = "file")]
    File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum MediaTagClipboard {
    #[default]
    #[serde(rename = "clip")]
    Clipboard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum MediaTagSite {
    #[default]
    #[serde(rename = "site")]
    Site,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum MediaTagVStat {
    #[default]
    #[serde(rename = "vstat")]
    VStat,
}

/// Why a display/camera route's stream isn't (or is again) producing
/// frames — the host's capture telling the viewer in-band, so "connected
/// but no pixels" reads as the real condition instead of a silent black
/// stage. Sent on state *changes* only; a peer that doesn't know the
/// `vstat` tag (or a state added since its build) drops the frame unread
/// ([`MediaPayload::decode`]'s unknown-tag rule).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoStatusState {
    /// Frames are flowing (clears any earlier condition).
    Ok,
    /// Wayland: the compositor's screen-share consent dialog is up on the
    /// host and needs a human there to approve it.
    WaitingConsent,
    /// The capture session is live but the display produces no frames —
    /// the monitor is asleep or blanked.
    DisplayAsleep,
    /// No capturable monitor right now (all outputs detached — deep-sleep
    /// DP monitors drop off the desktop entirely).
    NoMonitor,
    /// Grabs are failing (denied screen-recording permission, a portal
    /// that never granted, a locked session…) — `detail` carries the OS
    /// error text.
    GrabFailed,
    /// The camera a video route names isn't there (unplugged since the
    /// scan, and no other camera to fall back to).
    NoCamera,
    /// The camera exists but won't open — typically held by another app
    /// (V4L2's EBUSY, a Windows exclusive lock) or an OS camera-permission
    /// denial. `detail` carries the OS error text.
    CameraFailed,
    /// A status a newer build introduced. Reads as "not Ok" (the safe side:
    /// the stage shows as not-flowing rather than the frame failing to
    /// decode), so a future condition can never drop a status frame.
    Unknown,
}

// Lenient decode: a status this build doesn't recognise reads as `Unknown`
// rather than erroring (`#[serde(other)]` isn't available on a string-valued
// enum). `Unknown` is deliberately not `Ok`, so the viewer treats the stage
// as not-flowing — the safe default for an unknown condition.
impl<'de> Deserialize<'de> for VideoStatusState {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(match String::deserialize(d)?.as_str() {
            "ok" => VideoStatusState::Ok,
            "waiting_consent" => VideoStatusState::WaitingConsent,
            "display_asleep" => VideoStatusState::DisplayAsleep,
            "no_monitor" => VideoStatusState::NoMonitor,
            "grab_failed" => VideoStatusState::GrabFailed,
            "no_camera" => VideoStatusState::NoCamera,
            "camera_failed" => VideoStatusState::CameraFailed,
            _ => VideoStatusState::Unknown,
        })
    }
}

/// One capture-status change of a display route, host → viewer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoStatusFrame {
    /// Tag for demuxing off the shared media channel. Always `"vstat"`.
    pub t: MediaTagVStat,
    pub route: String,
    pub state: VideoStatusState,
    /// OS-level error text, when there is one worth showing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl VideoStatusFrame {
    pub fn new(route: impl Into<String>, state: VideoStatusState, detail: Option<String>) -> Self {
        VideoStatusFrame {
            t: MediaTagVStat::VStat,
            route: route.into(),
            state,
            detail,
        }
    }
}

impl VideoFrame {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        route: impl Into<String>,
        seq: u64,
        width: u32,
        height: u32,
        source_width: u32,
        source_height: u32,
        jpeg: Vec<u8>,
    ) -> Self {
        VideoFrame {
            t: MediaTagVideo::Video,
            route: route.into(),
            seq,
            width,
            height,
            source_width,
            source_height,
            chunk: 0,
            chunks: 1,
            jpeg,
        }
    }

    /// Split this frame into channel-sized pieces: each piece's JPEG slice
    /// is at most `max_jpeg_bytes`, so the full JSON message (base64 +
    /// envelope) stays under the transport's message ceiling. A frame that
    /// already fits comes back whole.
    pub fn into_chunks(self, max_jpeg_bytes: usize) -> Vec<VideoFrame> {
        let max = max_jpeg_bytes.max(1);
        if self.jpeg.len() <= max {
            return vec![self];
        }
        let pieces: Vec<&[u8]> = self.jpeg.chunks(max).collect();
        let total = pieces.len() as u16;
        pieces
            .into_iter()
            .enumerate()
            .map(|(i, piece)| VideoFrame {
                t: MediaTagVideo::Video,
                route: self.route.clone(),
                seq: self.seq,
                width: self.width,
                height: self.height,
                source_width: self.source_width,
                source_height: self.source_height,
                chunk: i as u16,
                chunks: total,
                jpeg: piece.to_vec(),
            })
            .collect()
    }
}

/// Reassembles chunked [`VideoFrame`]s per route. Frames are independent:
/// a newer `seq` discards any half-built older frame (that frame is simply
/// lost, never shown torn), and hostile shapes (absurd chunk counts,
/// out-of-range indices, ballooning totals) are dropped wholesale.
#[derive(Debug, Default)]
pub struct VideoAssembler {
    partial: std::collections::HashMap<String, PartialFrame>,
}

#[derive(Debug)]
struct PartialFrame {
    seq: u64,
    parts: Vec<Option<Vec<u8>>>,
    received: u16,
}

/// Upper bounds a frame may occupy mid-assembly — far above anything the
/// sender produces, low enough that a misbehaving peer can't balloon us.
const MAX_CHUNKS: u16 = 64;
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

impl VideoAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one inbound frame (or chunk). Returns the complete frame once
    /// every piece has arrived — immediately, for unchunked frames.
    pub fn push(&mut self, frame: VideoFrame) -> Option<VideoFrame> {
        if frame.chunks <= 1 {
            self.partial.remove(&frame.route);
            return Some(frame);
        }
        if frame.chunks > MAX_CHUNKS || frame.chunk >= frame.chunks {
            return None;
        }
        let entry = self.partial.entry(frame.route.clone());
        let p = match entry {
            std::collections::hash_map::Entry::Occupied(mut o) => {
                if o.get().seq < frame.seq || o.get().parts.len() != frame.chunks as usize {
                    *o.get_mut() = PartialFrame::new(frame.seq, frame.chunks);
                } else if o.get().seq > frame.seq {
                    return None; // a stale chunk of an abandoned frame
                }
                o.into_mut()
            }
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(PartialFrame::new(frame.seq, frame.chunks))
            }
        };
        let slot = &mut p.parts[frame.chunk as usize];
        if slot.is_none() {
            p.received += 1;
        }
        *slot = Some(frame.jpeg);
        let assembled_len: usize = p.parts.iter().flatten().map(Vec::len).sum();
        if assembled_len > MAX_FRAME_BYTES {
            self.partial.remove(&frame.route);
            return None;
        }
        if p.received < frame.chunks {
            return None;
        }
        let p = self.partial.remove(&frame.route)?;
        let mut jpeg = Vec::with_capacity(assembled_len);
        for part in p.parts.into_iter().flatten() {
            jpeg.extend_from_slice(&part);
        }
        Some(VideoFrame {
            t: MediaTagVideo::Video,
            route: frame.route,
            seq: frame.seq,
            width: frame.width,
            height: frame.height,
            source_width: frame.source_width,
            source_height: frame.source_height,
            chunk: 0,
            chunks: 1,
            jpeg,
        })
    }
}

impl PartialFrame {
    fn new(seq: u64, chunks: u16) -> Self {
        PartialFrame {
            seq,
            parts: vec![None; chunks as usize],
            received: 0,
        }
    }
}

impl InputEvent {
    pub fn new(route: impl Into<String>, seq: u64, action: InputAction) -> Self {
        InputEvent {
            t: MediaTagInput::Input,
            route: route.into(),
            seq,
            action,
        }
    }
}

mod bytes_b64 {
    use super::*;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(d)?;
        base64::engine::general_purpose::STANDARD
            .decode(text.as_bytes())
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_frame_round_trips_with_tag_and_b64() {
        let f = VideoFrame::new("route:a→b", 3, 640, 360, 1920, 1080, vec![0xFF, 0xD8, 0xFF]);
        let json = serde_json::to_string(&f).unwrap();
        assert!(json.contains("\"t\":\"video\""));
        assert!(json.contains("\"jpeg\":\""), "bytes travel as base64");
        let back: VideoFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(f, back);
    }

    #[test]
    fn input_event_round_trips_each_action() {
        let actions = [
            InputAction::MouseMove {
                x: 0.25,
                y: 0.75,
                screen: None,
            },
            InputAction::MouseMove {
                x: 0.5,
                y: 0.5,
                screen: Some(131_073),
            },
            InputAction::MouseButton {
                button: 2,
                down: true,
            },
            InputAction::Wheel { dx: 0.0, dy: -3.0 },
            InputAction::Key {
                key: "Enter".into(),
                code: None,
                down: false,
            },
            InputAction::Key {
                key: "c".into(),
                code: Some("KeyC".into()),
                down: true,
            },
        ];
        for action in actions {
            let e = InputEvent::new("r1", 9, action);
            let v = serde_json::to_value(&e).unwrap();
            assert_eq!(v["t"], "input");
            let back: InputEvent = serde_json::from_value(v).unwrap();
            assert_eq!(e, back);
        }
    }

    #[test]
    fn video_status_round_trips_and_demuxes() {
        let s = VideoStatusFrame::new(
            "route:a→b",
            VideoStatusState::WaitingConsent,
            Some("portal dialog pending".into()),
        );
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["t"], "vstat");
        assert_eq!(v["state"], "waiting_consent");
        assert!(matches!(
            MediaPayload::decode(v),
            Some(MediaPayload::VideoStatus(f))
                if f.route == "route:a→b" && f.state == VideoStatusState::WaitingConsent
        ));

        // `Ok` with no detail keeps the wire shape minimal.
        let ok =
            serde_json::to_value(VideoStatusFrame::new("r", VideoStatusState::Ok, None)).unwrap();
        assert!(ok.get("detail").is_none());
        assert_eq!(ok["state"], "ok");

        // The camera conditions ride the same frame; a peer that predates
        // them fails the decode and drops the frame, per the tag rule.
        let cam = serde_json::to_value(VideoStatusFrame::new(
            "r",
            VideoStatusState::CameraFailed,
            Some("Device or resource busy".into()),
        ))
        .unwrap();
        assert_eq!(cam["state"], "camera_failed");
        assert!(matches!(
            MediaPayload::decode(cam),
            Some(MediaPayload::VideoStatus(f)) if f.state == VideoStatusState::CameraFailed
        ));
        let none =
            serde_json::to_value(VideoStatusFrame::new("r", VideoStatusState::NoCamera, None))
                .unwrap();
        assert_eq!(none["state"], "no_camera");
    }

    #[test]
    fn demux_dispatches_on_the_tag_with_audio_as_the_untagged_default() {
        let audio = serde_json::to_value(AudioFrame::new("r", 1, 48_000, 1, vec![1, 2])).unwrap();
        assert!(matches!(
            MediaPayload::decode(audio),
            Some(MediaPayload::Audio(f)) if f.route == "r"
        ));

        let video = serde_json::to_value(VideoFrame::new("r", 1, 8, 8, 8, 8, vec![1])).unwrap();
        assert!(matches!(
            MediaPayload::decode(video),
            Some(MediaPayload::Video(_))
        ));

        let input = serde_json::to_value(InputEvent::new(
            "r",
            1,
            InputAction::Key {
                key: "a".into(),
                code: None,
                down: true,
            },
        ))
        .unwrap();
        assert!(matches!(
            MediaPayload::decode(input),
            Some(MediaPayload::Input(_))
        ));

        let term =
            serde_json::to_value(TermFrame::new("r", 1, TermEvent::Data { bytes: vec![27] }))
                .unwrap();
        assert!(matches!(
            MediaPayload::decode(term),
            Some(MediaPayload::Terminal(_))
        ));

        // A future kind we don't know is dropped, not an error.
        assert_eq!(
            MediaPayload::decode(serde_json::json!({ "t": "hologram", "route": "r" })),
            None
        );
    }

    #[test]
    fn term_frame_round_trips_each_event() {
        let events = [
            TermEvent::Data {
                bytes: b"ls -la\r".to_vec(),
            },
            TermEvent::Resize {
                cols: 120,
                rows: 40,
            },
            TermEvent::Exit { code: Some(3) },
            TermEvent::Exit { code: None },
        ];
        for event in events {
            let f = TermFrame::new("route:a:terminal→b:term-view:1", 9, event);
            let v = serde_json::to_value(&f).unwrap();
            assert_eq!(v["t"], "term");
            let back: TermFrame = serde_json::from_value(v).unwrap();
            assert_eq!(f, back);
        }
        // Data bytes travel as base64, never raw.
        let f = TermFrame::new("r", 0, TermEvent::Data { bytes: vec![0xFF] });
        let json = serde_json::to_string(&f).unwrap();
        assert!(json.contains("\"bytes\":\"/w==\""));
    }

    #[test]
    fn file_frame_round_trips_each_event() {
        let events = [
            FileEvent::Quota { req: 11 },
            FileEvent::List {
                req: 1,
                path: "~".into(),
                cursor: None,
                limit: None,
            },
            FileEvent::Read {
                req: 2,
                path: "/home/u/notes.txt".into(),
            },
            FileEvent::Write {
                req: 3,
                path: "/home/u/up.bin".into(),
                data: vec![0xFF, 0x00],
                append: true,
                eof: true,
            },
            FileEvent::Mkdir {
                req: 4,
                path: "/home/u/new".into(),
            },
            FileEvent::Rename {
                req: 5,
                from: "/home/u/a".into(),
                to: "/home/u/b".into(),
            },
            FileEvent::Delete {
                req: 6,
                path: "/home/u/old".into(),
            },
            FileEvent::Entries {
                req: 7,
                path: "/home/u".into(),
                home: "/home/u".into(),
                entries: vec![FileEntry {
                    name: "docs".into(),
                    native_id: Some("unix:1:2".into()),
                    hidden: false,
                    dir: true,
                    size: 0,
                    modified: Some(1_700_000_000),
                    symlink: false,
                }],
                next_cursor: None,
            },
            FileEvent::Chunk {
                req: 8,
                data: vec![1, 2, 3],
                total: 3,
                eof: true,
            },
            FileEvent::Ok { req: 9 },
            FileEvent::Err {
                req: 10,
                reason: "no such file".into(),
            },
            FileEvent::QuotaInfo {
                req: 12,
                used: 40,
                total: 100,
            },
        ];
        for event in events {
            let req = event.req();
            let f = FileFrame::new("route:a:files→b:files-view:1", 9, event);
            let v = serde_json::to_value(&f).unwrap();
            assert_eq!(v["t"], "file");
            assert_eq!(v["req"], req);
            let back: FileFrame = serde_json::from_value(v).unwrap();
            assert_eq!(f, back);
        }
        // Bytes travel as base64, never raw.
        let f = FileFrame::new(
            "r",
            0,
            FileEvent::Chunk {
                req: 1,
                data: vec![0xFF],
                total: 1,
                eof: false,
            },
        );
        let json = serde_json::to_string(&f).unwrap();
        assert!(json.contains("\"data\":\"/w==\""));
    }

    #[test]
    fn file_frames_demux_and_unknown_kinds_drop_alone() {
        let ok = serde_json::to_value(FileFrame::new(
            "r",
            5,
            FileEvent::List {
                req: 1,
                path: "~".into(),
                cursor: None,
                limit: None,
            },
        ))
        .unwrap();
        assert!(matches!(
            MediaPayload::decode(ok),
            Some(MediaPayload::File(f)) if f.seq == 5
        ));
        // A newer peer's new event kind decodes to `Unknown` (the frame
        // still parses) and is ignored by the handler — it never fails the
        // decode and so can't take the route's other frames down with it.
        assert!(matches!(
            MediaPayload::decode(serde_json::json!({
                "t": "file", "route": "r", "seq": 4, "kind": "teleport"
            })),
            Some(MediaPayload::File(f)) if f.event == FileEvent::Unknown
        ));
    }

    #[test]
    fn clipboard_frames_round_trip_and_demux() {
        let events = vec![
            ClipboardEvent::Text {
                text: "hello clipboard".into(),
            },
            ClipboardEvent::Open {
                transfer: 7,
                content: ClipboardContentKind::Files,
                items: vec![
                    ClipboardItem {
                        name: "a.txt".into(),
                        size: 3,
                    },
                    ClipboardItem {
                        name: "b.png".into(),
                        size: 9,
                    },
                ],
            },
            ClipboardEvent::Chunk {
                transfer: 7,
                item: 1,
                data: vec![0xFF, 0x00, 0x10],
            },
            ClipboardEvent::Close { transfer: 7 },
            ClipboardEvent::Applied {
                transfer: 7,
                error: None,
            },
            // Copy/cut-from-remote request — a fieldless variant.
            ClipboardEvent::Pull,
        ];
        for event in events {
            let f = ClipboardFrame::new("route:a:clipboard→b:clipboard", 3, event);
            let v = serde_json::to_value(&f).unwrap();
            assert_eq!(v["t"], "clip");
            let back: ClipboardFrame = serde_json::from_value(v.clone()).unwrap();
            assert_eq!(f, back);
            assert!(matches!(
                MediaPayload::decode(v),
                Some(MediaPayload::Clipboard(g)) if g.seq == 3
            ));
        }
        // Pull is the bare `{kind: "pull"}` on the wire (no fields to carry).
        let pull = ClipboardFrame::new("r", 0, ClipboardEvent::Pull);
        assert_eq!(serde_json::to_value(&pull).unwrap()["kind"], "pull");
        // Chunk bytes travel as base64, never raw.
        let f = ClipboardFrame::new(
            "r",
            0,
            ClipboardEvent::Chunk {
                transfer: 1,
                item: 0,
                data: vec![0xFF],
            },
        );
        assert!(serde_json::to_string(&f)
            .unwrap()
            .contains("\"data\":\"/w==\""));
        // A newer peer's new event kind decodes to `Unknown` rather than
        // failing the frame (handlers ignore it).
        assert!(matches!(
            MediaPayload::decode(serde_json::json!({
                "t": "clip", "route": "r", "seq": 1, "kind": "hologram"
            })),
            Some(MediaPayload::Clipboard(g)) if g.event == ClipboardEvent::Unknown
        ));
    }

    #[test]
    fn site_frames_round_trip_and_demux() {
        let events = vec![
            SiteEvent::Open {
                conn: 1,
                port: 8080,
            },
            SiteEvent::Data {
                conn: 1,
                data: vec![0x47, 0x45, 0x54],
            },
            SiteEvent::Close { conn: 1 },
        ];
        for event in events {
            let conn = event.conn();
            let f = SiteFrame::new("route:a:site→b:site-view:0", 2, event);
            let v = serde_json::to_value(&f).unwrap();
            assert_eq!(v["t"], "site");
            assert_eq!(v["conn"], conn);
            let back: SiteFrame = serde_json::from_value(v.clone()).unwrap();
            assert_eq!(f, back);
            assert!(matches!(
                MediaPayload::decode(v),
                Some(MediaPayload::Site(g)) if g.seq == 2
            ));
        }
        // Tunneled bytes travel as base64, never raw.
        let f = SiteFrame::new(
            "r",
            0,
            SiteEvent::Data {
                conn: 9,
                data: vec![0xFF],
            },
        );
        assert!(serde_json::to_string(&f)
            .unwrap()
            .contains("\"data\":\"/w==\""));
        // A newer peer's new event kind decodes to `Unknown` rather than
        // failing the frame (handlers ignore it).
        assert!(matches!(
            MediaPayload::decode(serde_json::json!({
                "t": "site", "route": "r", "seq": 1, "kind": "warp"
            })),
            Some(MediaPayload::Site(g)) if g.event == SiteEvent::Unknown
        ));
    }

    #[test]
    fn term_frame_with_an_unknown_kind_decodes_to_unknown() {
        // A newer peer's new event kind decodes to `Unknown` (the frame
        // still parses) and the handler ignores it — the stream's surviving
        // frames are unaffected, and one new kind can't fail the decode.
        assert!(matches!(
            MediaPayload::decode(serde_json::json!({
                "t": "term", "route": "r", "seq": 4, "kind": "hologram"
            })),
            Some(MediaPayload::Terminal(f)) if f.event == TermEvent::Unknown
        ));
        let ok = serde_json::to_value(TermFrame::new("r", 5, TermEvent::Data { bytes: vec![1] }))
            .unwrap();
        assert!(matches!(
            MediaPayload::decode(ok),
            Some(MediaPayload::Terminal(f)) if f.seq == 5
        ));
    }

    #[test]
    fn term_data_chunks_split_byte_exact_and_number_sequentially() {
        let bytes: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
        let frames = TermFrame::data_frames("r", 7, &bytes, 16 * 1024);
        assert_eq!(frames.len(), 4);
        let mut rebuilt = Vec::new();
        for (i, f) in frames.iter().enumerate() {
            assert_eq!(f.seq, 7 + i as u64);
            match &f.event {
                TermEvent::Data { bytes } => {
                    assert!(bytes.len() <= 16 * 1024);
                    rebuilt.extend_from_slice(bytes);
                }
                other => panic!("expected Data, got {other:?}"),
            }
        }
        assert_eq!(rebuilt, bytes);

        // A small write passes through as exactly one frame; an empty one
        // still yields a frame rather than vanishing.
        assert_eq!(TermFrame::data_frames("r", 0, b"hi", 16 * 1024).len(), 1);
        assert_eq!(TermFrame::data_frames("r", 0, b"", 16 * 1024).len(), 1);
    }

    #[test]
    fn chunking_splits_big_frames_and_reassembles_them_byte_exact() {
        let jpeg: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let frame = VideoFrame::new("r", 7, 1280, 720, 1920, 1080, jpeg.clone());
        let chunks = frame.into_chunks(40_000);
        assert_eq!(chunks.len(), 3);
        assert!(chunks
            .iter()
            .all(|c| c.jpeg.len() <= 40_000 && c.chunks == 3));

        let mut asm = VideoAssembler::new();
        // Out-of-order arrival still assembles.
        assert!(asm.push(chunks[2].clone()).is_none());
        assert!(asm.push(chunks[0].clone()).is_none());
        let full = asm.push(chunks[1].clone()).expect("complete");
        assert_eq!(full.jpeg, jpeg);
        assert_eq!((full.seq, full.chunk, full.chunks), (7, 0, 1));

        // A small frame passes through untouched and unsplit.
        let small = VideoFrame::new("r", 8, 8, 8, 8, 8, vec![1, 2, 3]);
        assert_eq!(small.clone().into_chunks(40_000).len(), 1);
        assert_eq!(asm.push(small).unwrap().jpeg, vec![1, 2, 3]);
    }

    #[test]
    fn a_newer_frame_discards_a_half_built_older_one() {
        let old = VideoFrame::new("r", 1, 8, 8, 8, 8, vec![0u8; 100]).into_chunks(40);
        let new = VideoFrame::new("r", 2, 8, 8, 8, 8, vec![1u8; 100]).into_chunks(40);
        let mut asm = VideoAssembler::new();
        assert!(asm.push(old[0].clone()).is_none());
        // The newer frame arrives; the old partial is abandoned.
        for c in &new[..new.len() - 1] {
            assert!(asm.push(c.clone()).is_none());
        }
        // A stale chunk of the abandoned frame can't corrupt the new one.
        assert!(asm.push(old[1].clone()).is_none());
        let full = asm
            .push(new[new.len() - 1].clone())
            .expect("new frame completes");
        assert!(full.jpeg.iter().all(|&b| b == 1));
    }

    #[test]
    fn hostile_chunk_shapes_are_dropped() {
        let mut asm = VideoAssembler::new();
        let mut absurd = VideoFrame::new("r", 1, 8, 8, 8, 8, vec![0]);
        absurd.chunks = 60_000;
        absurd.chunk = 5;
        assert!(asm.push(absurd).is_none());

        let mut out_of_range = VideoFrame::new("r", 1, 8, 8, 8, 8, vec![0]);
        out_of_range.chunks = 4;
        out_of_range.chunk = 4; // index == count
        assert!(asm.push(out_of_range).is_none());
    }

    #[test]
    fn a_screenless_mouse_move_still_decodes() {
        // The exact shape an older sender (no per-screen control) emits.
        let legacy = serde_json::json!({
            "t": "input", "route": "r", "seq": 1,
            "kind": "mouse_move", "x": 0.5, "y": 0.5
        });
        let ev: InputEvent = serde_json::from_value(legacy).expect("legacy decodes");
        assert_eq!(
            ev.action,
            InputAction::MouseMove {
                x: 0.5,
                y: 0.5,
                screen: None
            }
        );
        // And the screenless shape serializes without the field, so an
        // older *receiver* isn't handed a key it never knew.
        let v = serde_json::to_value(&ev).unwrap();
        assert!(v.get("screen").is_none());
    }

    #[test]
    fn a_codeless_key_event_still_decodes() {
        // The exact shape an older sender (no physical-key field) emits.
        let legacy = serde_json::json!({
            "t": "input", "route": "r", "seq": 1,
            "kind": "key", "key": "Enter", "down": true
        });
        let ev: InputEvent = serde_json::from_value(legacy).expect("legacy decodes");
        assert_eq!(
            ev.action,
            InputAction::Key {
                key: "Enter".into(),
                code: None,
                down: true
            }
        );
        // And the codeless shape serializes without the field, so an
        // older *receiver* isn't handed a key it never knew.
        let v = serde_json::to_value(&ev).unwrap();
        assert!(v.get("code").is_none());
    }

    #[test]
    fn a_v1_audio_frame_still_decodes_unchanged() {
        // The exact shape a v0.1.0 peer sends — no tag.
        let legacy = serde_json::json!({
            "route": "route:mic→spk", "seq": 4, "sample_rate": 44_100,
            "channels": 1, "pcm": "AAABAA=="
        });
        let p = MediaPayload::decode(legacy).expect("legacy audio decodes");
        assert_eq!(p.route(), "route:mic→spk");
    }

    // ---- forward-compatibility: a newer build's frame kinds never break a
    // ---- consumer that doesn't recognise them ---------------------------

    #[test]
    fn unknown_flattened_event_kinds_decode_to_unknown() {
        // Every flattened media event enum must tolerate a kind a future
        // build adds, decoding to `Unknown` rather than failing the frame —
        // which would otherwise drop the whole route's stream.
        let term: TermFrame = serde_json::from_value(
            serde_json::json!({ "t": "term", "route": "r", "seq": 0, "kind": "title" }),
        )
        .expect("unknown term event");
        assert_eq!(term.event, TermEvent::Unknown);

        let site: SiteFrame = serde_json::from_value(
            serde_json::json!({ "t": "site", "route": "r", "seq": 0, "kind": "reset" }),
        )
        .expect("unknown site event");
        assert_eq!(site.event, SiteEvent::Unknown);

        let input: InputEvent = serde_json::from_value(
            serde_json::json!({ "t": "input", "route": "r", "seq": 0, "kind": "gesture" }),
        )
        .expect("unknown input action");
        assert_eq!(input.action, InputAction::Unknown);

        let clip: ClipboardFrame = serde_json::from_value(
            serde_json::json!({ "t": "clip", "route": "r", "seq": 0, "kind": "html" }),
        )
        .expect("unknown clipboard event");
        assert_eq!(clip.event, ClipboardEvent::Unknown);
    }

    #[test]
    fn unknown_string_enums_fall_back_safely() {
        // A clipboard content kind a newer build adds doesn't fail the
        // `Open` decode…
        let open: ClipboardFrame = serde_json::from_value(serde_json::json!({
            "t": "clip", "route": "r", "seq": 0, "kind": "open",
            "transfer": 1, "content": "rich_text", "items": []
        }))
        .expect("unknown clipboard content kind");
        assert!(matches!(
            open.event,
            ClipboardEvent::Open {
                content: ClipboardContentKind::Unknown,
                ..
            }
        ));

        // …and a video status a newer build adds reads as "not Ok" rather
        // than dropping the status frame.
        let vstat: VideoStatusFrame = serde_json::from_value(serde_json::json!({
            "t": "vstat", "route": "r", "state": "throttled"
        }))
        .expect("unknown video status");
        assert_eq!(vstat.state, VideoStatusState::Unknown);
    }

    #[test]
    fn media_payload_still_drops_unknown_top_level_tag() {
        // The top-level demux already dropped unknown `t`s; keep that
        // contract (a brand-new media *plane* is ignored, never an error).
        let v = serde_json::json!({ "t": "hologram", "route": "r", "seq": 0 });
        assert!(MediaPayload::decode(v).is_none());
    }
}
