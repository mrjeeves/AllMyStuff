//! Cross-platform OS-clipboard access for cross-machine copy/paste.
//!
//! The bundled clipboard-manager plugin only reaches text, HTML and images;
//! copying *files* puts file references on the OS clipboard in a platform
//! format (CF_HDROP on Windows, file URLs on macOS, `text/uri-list` on
//! Linux) it can't read or write. `clipboard-rs` can, so this module owns
//! the clipboard for the copy/paste feature.
//!
//! All access runs on one dedicated thread that holds the single
//! [`ClipboardContext`] for the app's life. That matters on X11, where the
//! process that *set* the clipboard must stay alive to hand the data to
//! whoever pastes — a transient context would lose the selection the moment
//! it dropped. Keeping the context on its own thread also sidesteps the
//! `Send`/`Sync` question: it never leaves that thread.

use std::path::{Path, PathBuf};
use std::sync::mpsc::SyncSender;

use clipboard_rs::common::RustImage;
use clipboard_rs::{
    Clipboard, ClipboardContext, ClipboardHandler, ClipboardWatcher, ClipboardWatcherContext,
    ContentFormat, RustImageData,
};
use tokio::sync::mpsc::UnboundedSender;

/// One file referenced on the clipboard — its base name, real path on this
/// machine, and byte size. The bytes stream from `path` at paste time, never
/// held in memory.
#[derive(Debug, Clone)]
pub struct LocalFile {
    pub name: String,
    pub path: PathBuf,
    pub size: u64,
}

/// What was on this machine's clipboard when a paste fired.
#[derive(Debug, Clone)]
pub enum LocalClip {
    Text(String),
    /// A bitmap, already PNG-encoded.
    Image(Vec<u8>),
    /// Files by reference (bytes stream from disk).
    Files(Vec<LocalFile>),
}

enum Cmd {
    Read(SyncSender<Result<Option<LocalClip>, String>>),
    SetText(String, SyncSender<Result<(), String>>),
    SetImage(Vec<u8>, SyncSender<Result<(), String>>), // PNG bytes
    SetFiles(Vec<String>, SyncSender<Result<(), String>>),
}

impl LocalClip {
    /// A stable identity for "is this the same clipboard content?".
    ///
    /// The sync loop compares fingerprints to tell a change the *user* made
    /// from one it caused itself by applying the peer's clipboard — without
    /// which every sync would echo straight back and the two machines would
    /// bounce one copy between them forever.
    ///
    /// Files hash by path, not by content: a file's identity on the clipboard
    /// *is* its path, and hashing the bytes would mean reading every copied
    /// file on every clipboard change.
    pub fn fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        match self {
            LocalClip::Text(t) => {
                0u8.hash(&mut h);
                t.hash(&mut h);
            }
            LocalClip::Image(png) => {
                1u8.hash(&mut h);
                png.hash(&mut h);
            }
            LocalClip::Files(files) => {
                2u8.hash(&mut h);
                for f in files {
                    f.path.hash(&mut h);
                    f.size.hash(&mut h);
                }
            }
        }
        h.finish()
    }
}

/// Bridges the OS clipboard's own change notification onto a broadcast the
/// engine can await. The handler must do nothing but signal: it runs on the
/// watcher thread, inside the platform's clipboard callback, and reading the
/// clipboard from there would deadlock against the context that owns it.
struct ChangeHandler {
    tx: tokio::sync::broadcast::Sender<()>,
}

impl ClipboardHandler for ChangeHandler {
    fn on_clipboard_change(&mut self) {
        // Err just means nobody is listening yet — a clipboard change with no
        // live sync route is not news.
        let _ = self.tx.send(());
    }
}

/// Handle to the clipboard thread. Cheap to clone (just the command sender).
/// A tokio sender so it's `Send + Sync` — `Mesh` holds it inside an `Arc`.
#[derive(Clone)]
pub struct ClipboardService {
    tx: UnboundedSender<Cmd>,
    /// Fires whenever the OS clipboard changes, from the platform's own
    /// notification rather than a poll — so a sync costs nothing while
    /// nobody is copying, and an image never gets re-encoded just to notice
    /// it hasn't changed.
    changes: tokio::sync::broadcast::Sender<()>,
}

impl ClipboardService {
    /// Spawn the clipboard thread. Always returns a handle; if the OS
    /// clipboard can't be opened (a headless box, no display), reads return
    /// `None` and writes are dropped — never a panic.
    pub fn spawn() -> ClipboardService {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Cmd>();
        std::thread::Builder::new()
            .name("clipboard".into())
            .spawn(move || {
                let ctx = match ClipboardContext::new() {
                    Ok(c) => Some(c),
                    Err(e) => {
                        tracing::warn!("OS clipboard unavailable: {e}");
                        None
                    }
                };
                while let Some(cmd) = rx.blocking_recv() {
                    let Some(ctx) = ctx.as_ref() else {
                        match cmd {
                            Cmd::Read(resp) => {
                                let _ = resp.send(Err("OS clipboard is unavailable".into()));
                            }
                            Cmd::SetText(_, resp)
                            | Cmd::SetImage(_, resp)
                            | Cmd::SetFiles(_, resp) => {
                                let _ = resp.send(Err("OS clipboard is unavailable".into()));
                            }
                        }
                        continue;
                    };
                    match cmd {
                        Cmd::Read(resp) => {
                            let _ = resp.send(read_clipboard(ctx));
                        }
                        Cmd::SetText(t, resp) => {
                            let result = ctx.set_text(t).map_err(|e| e.to_string());
                            let _ = resp.send(result);
                        }
                        Cmd::SetImage(png, resp) => {
                            let result = RustImageData::from_bytes(&png)
                                .map_err(|e| e.to_string())
                                .and_then(|img| ctx.set_image(img).map_err(|e| e.to_string()));
                            let _ = resp.send(result);
                        }
                        Cmd::SetFiles(paths, resp) => {
                            let result = ctx.set_files(paths).map_err(|e| e.to_string());
                            let _ = resp.send(result);
                        }
                    }
                }
            })
            .expect("spawn clipboard thread");

        // A second thread for the platform's change notification. It needs its
        // own context (`start_watch` blocks for the app's life) and must not
        // touch the one above — reads still go through the command thread, so
        // the single-context rule that keeps an X11 selection served is intact.
        // Capacity 8 because a receiver only ever needs to know *that* the
        // clipboard changed; lagging past it collapses to one wake-up, which
        // is the right answer anyway.
        let (changes, _) = tokio::sync::broadcast::channel(8);
        let watch_tx = changes.clone();
        std::thread::Builder::new()
            .name("clipboard-watch".into())
            .spawn(move || match ClipboardWatcherContext::new() {
                Ok(mut watcher) => {
                    watcher.add_handler(ChangeHandler { tx: watch_tx });
                    // Blocks until shutdown; this thread exists to sit here.
                    watcher.start_watch();
                }
                // No watcher (headless, no display) means no sync — the
                // explicit copy/paste path still works, so this is a
                // degradation, not a failure.
                Err(e) => tracing::warn!("clipboard change watcher unavailable: {e}"),
            })
            .expect("spawn clipboard watcher thread");

        ClipboardService { tx, changes }
    }

    /// Await OS clipboard changes. Each subscriber gets its own receiver; a
    /// receiver that falls behind is fine, since every message means the same
    /// thing ("go look").
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<()> {
        self.changes.subscribe()
    }

    /// Read this machine's clipboard. Blocking — call from a blocking
    /// context (the mesh wraps it in `spawn_blocking`). `Ok(None)` means the
    /// clipboard is empty or contains no supported kind; real native read
    /// failures stay errors so an explicit paste can tell the user.
    pub fn read(&self) -> Result<Option<LocalClip>, String> {
        let (resp_tx, resp_rx) = std::sync::mpsc::sync_channel(1);
        self.tx
            .send(Cmd::Read(resp_tx))
            .map_err(|_| "clipboard service stopped".to_string())?;
        resp_rx
            .recv()
            .map_err(|_| "clipboard service stopped".to_string())?
    }

    pub fn set_text(&self, text: String) -> Result<(), String> {
        self.write(|resp| Cmd::SetText(text, resp))
    }

    /// Set the clipboard to a PNG image (decoded on the clipboard thread).
    pub fn set_image(&self, png: Vec<u8>) -> Result<(), String> {
        self.write(|resp| Cmd::SetImage(png, resp))
    }

    /// Point the clipboard at real files on this machine, so a paste in a
    /// file manager materializes them.
    pub fn set_files(&self, paths: Vec<String>) -> Result<(), String> {
        self.write(|resp| Cmd::SetFiles(paths, resp))
    }

    /// Run one clipboard write to completion. Receiving a clipboard `Close`
    /// must not return while the OS write is merely queued: the remote paste
    /// key follows it on the ordered media channel, and image decoding/file
    /// list publication can take long enough for that key to otherwise paste
    /// the previous clipboard contents.
    fn write(&self, cmd: impl FnOnce(SyncSender<Result<(), String>>) -> Cmd) -> Result<(), String> {
        let (resp_tx, resp_rx) = std::sync::mpsc::sync_channel(1);
        self.tx
            .send(cmd(resp_tx))
            .map_err(|_| "clipboard service stopped".to_string())?;
        resp_rx
            .recv()
            .map_err(|_| "clipboard service stopped".to_string())?
    }
}

/// Resolve paths supplied by a native file-drop event into the same streaming
/// descriptors used by an OS clipboard file list. Paths never cross a UI file
/// input (which intentionally hides them); the Tauri webview gives these to the
/// trusted backend directly.
pub fn local_files(paths: Vec<String>) -> Result<Vec<LocalFile>, String> {
    let mut files = Vec::with_capacity(paths.len());
    let mut names = std::collections::HashSet::new();
    for raw in paths {
        // Tauri normally supplies a native path, but macOS can surface a
        // file URL for some Finder/pasteboard drags. Accept both forms here.
        let path = normalize_clip_path(&raw);
        let meta = std::fs::metadata(&path)
            .map_err(|e| format!("can't read {}: {e}", path.to_string_lossy()))?;
        if !meta.is_file() {
            return Err(format!(
                "folders aren't supported yet: {}",
                path.to_string_lossy()
            ));
        }
        let name = base_name(&path);
        if !names.insert(name.clone()) {
            return Err(format!("more than one dropped file is named {name}"));
        }
        files.push(LocalFile {
            name,
            path,
            size: meta.len(),
        });
    }
    if files.is_empty() {
        return Err("no files were dropped".into());
    }
    Ok(files)
}

/// Query the clipboard, preferring files, then an image, then text — the
/// order that keeps a file copy from degrading to a text-path label.
fn read_clipboard(ctx: &ClipboardContext) -> Result<Option<LocalClip>, String> {
    let mut failures = Vec::new();
    if ctx.has(ContentFormat::Files) {
        match ctx.get_files() {
            Ok(raw) => {
                let files: Vec<LocalFile> = raw
                    .iter()
                    .filter_map(|entry| {
                        let path = normalize_clip_path(entry);
                        let meta = std::fs::metadata(&path).ok()?;
                        if !meta.is_file() {
                            return None; // directories are a follow-up
                        }
                        Some(LocalFile {
                            name: base_name(&path),
                            path,
                            size: meta.len(),
                        })
                    })
                    .collect();
                if !files.is_empty() {
                    return Ok(Some(LocalClip::Files(files)));
                }
                if !raw.is_empty() {
                    failures.push(
                        "the clipboard named files, but none were readable regular files".into(),
                    );
                }
            }
            Err(e) => failures.push(format!("native file list: {e}")),
        }
    }
    if ctx.has(ContentFormat::Image) {
        match ctx.get_image() {
            Ok(img) => match img.to_png() {
                Ok(png) => return Ok(Some(LocalClip::Image(png.get_bytes().to_vec()))),
                Err(e) => failures.push(format!("encode clipboard image: {e}")),
            },
            Err(e) => failures.push(format!("clipboard image: {e}")),
        }
    } else {
        // clipboard-rs' AppKit image check covers PNG/TIFF. Some macOS
        // applications publish JPEG/WebP (and browsers may expose MIME names
        // instead of UTIs), so try those advertised byte representations too.
        const RAW_IMAGE_FORMATS: &[&str] = &[
            "public.png",
            "public.tiff",
            "public.jpeg",
            "public.jpg",
            "public.webp",
            "image/png",
            "image/tiff",
            "image/jpeg",
            "image/webp",
        ];
        if let Ok(formats) = ctx.available_formats() {
            for format in formats
                .iter()
                .filter(|format| RAW_IMAGE_FORMATS.contains(&format.as_str()))
            {
                match ctx
                    .get_buffer(format)
                    .map_err(|e| e.to_string())
                    .and_then(|bytes| RustImageData::from_bytes(&bytes).map_err(|e| e.to_string()))
                    .and_then(|img| img.to_png().map_err(|e| e.to_string()))
                {
                    Ok(png) => return Ok(Some(LocalClip::Image(png.get_bytes().to_vec()))),
                    Err(e) => failures.push(format!("clipboard {format}: {e}")),
                }
            }
        }
    }
    if ctx.has(ContentFormat::Text) {
        match ctx.get_text() {
            Ok(text) if !text.is_empty() => return Ok(Some(LocalClip::Text(text))),
            Ok(_) => {}
            Err(e) => failures.push(format!("clipboard text: {e}")),
        }
    }
    if failures.is_empty() {
        Ok(None)
    } else {
        let formats = ctx
            .available_formats()
            .map(|formats| formats.join(", "))
            .unwrap_or_else(|_| "unknown".into());
        Err(format!(
            "couldn't read clipboard content ({}) [formats: {formats}]",
            failures.join("; ")
        ))
    }
}

/// Turn a clipboard file entry into a real path: a `file://` URL (Linux /
/// macOS `text/uri-list`) is decoded; a bare path (Windows CF_HDROP) stands.
fn normalize_clip_path(entry: &str) -> PathBuf {
    let s = entry.trim();
    if let Some(rest) = s.strip_prefix("file://") {
        // Drop an optional host ("file://host/path" → "/path"), keeping the
        // path's own leading slash.
        let path = match rest.find('/') {
            Some(i) => &rest[i..],
            None => rest,
        };
        PathBuf::from(percent_decode(path))
    } else {
        PathBuf::from(s)
    }
}

/// Minimal percent-decoding for file URLs (`%20` → space, …). A malformed
/// escape is left untouched.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn base_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into())
}

/// The staging directory a received clipboard transfer lands in, so the OS
/// clipboard can point a paste at real files. Per-transfer so concurrent
/// pastes never collide; under the system temp dir.
pub fn staging_dir(transfer: u64) -> PathBuf {
    std::env::temp_dir()
        .join("allmystuff-clipboard")
        .join(transfer.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_urls_decode_to_paths() {
        assert_eq!(
            normalize_clip_path("file:///home/u/my%20file.txt"),
            PathBuf::from("/home/u/my file.txt")
        );
        // A bare path (Windows-style entry) is left alone.
        assert_eq!(
            normalize_clip_path("C:\\Users\\u\\a.txt"),
            PathBuf::from("C:\\Users\\u\\a.txt")
        );
        // A host component is dropped.
        assert_eq!(
            normalize_clip_path("file://host/srv/data.bin"),
            PathBuf::from("/srv/data.bin")
        );
    }

    #[test]
    fn base_name_is_the_final_component() {
        assert_eq!(base_name(Path::new("/a/b/c.png")), "c.png");
    }

    #[test]
    fn native_drops_resolve_files_and_reject_folders() {
        let root = std::env::temp_dir().join(format!(
            "allmystuff-clipboard-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("folder")).unwrap();
        let path = root.join("picture.png");
        std::fs::write(&path, b"png").unwrap();

        let files = local_files(vec![path.to_string_lossy().into_owned()]).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "picture.png");
        assert_eq!(files[0].size, 3);
        assert!(local_files(vec![root.join("folder").to_string_lossy().into_owned()]).is_err());

        let _ = std::fs::remove_dir_all(root);
    }

    /// The fingerprint is what stops a synced clipboard ping-ponging between
    /// two machines forever, so it has to be stable for identical content and
    /// different for anything a user would call a different copy.
    #[test]
    fn the_fingerprint_identifies_content_not_the_moment_it_was_read() {
        let a = LocalClip::Text("hello".into());
        let b = LocalClip::Text("hello".into());
        let c = LocalClip::Text("hello ".into());
        assert_eq!(a.fingerprint(), b.fingerprint(), "same text, same identity");
        assert_ne!(
            a.fingerprint(),
            c.fingerprint(),
            "a trailing space is a different copy"
        );

        // Kinds never collide, even when their payloads look alike — an image
        // whose bytes spell a string must not read as that string.
        let img = LocalClip::Image(b"hello".to_vec());
        assert_ne!(a.fingerprint(), img.fingerprint());

        // Files hash by path + size, so re-copying the same files is the same
        // clipboard and won't be forwarded back around the loop.
        let f = |name: &str, size: u64| LocalFile {
            name: name.into(),
            path: PathBuf::from(format!("/tmp/{name}")),
            size,
        };
        let one = LocalClip::Files(vec![f("a.txt", 10), f("b.txt", 20)]);
        let same = LocalClip::Files(vec![f("a.txt", 10), f("b.txt", 20)]);
        let reordered = LocalClip::Files(vec![f("b.txt", 20), f("a.txt", 10)]);
        let resized = LocalClip::Files(vec![f("a.txt", 11), f("b.txt", 20)]);
        assert_eq!(one.fingerprint(), same.fingerprint());
        assert_ne!(
            one.fingerprint(),
            resized.fingerprint(),
            "an edited file is a new copy"
        );
        // Order is part of the identity — the OS hands back what it was given,
        // so a differently-ordered selection is genuinely a different clipboard
        // and re-syncing it is correct, not an echo.
        assert_ne!(one.fingerprint(), reordered.fingerprint());
    }
}
