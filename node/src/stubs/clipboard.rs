//! No-op twin of [`crate::clipboard`] for capture-less builds
//! (`--no-default-features`, i.e. iOS — see the `host` feature in
//! `Cargo.toml`).
//!
//! `clipboard-rs` has no iOS backend (UIPasteboard is a webview/UIKit
//! concern the mobile GUI can grow later), so reads report unavailable and
//! writes drop — the exact posture the desktop service already takes on a
//! headless box whose OS clipboard won't open. [`staging_dir`] stays real:
//! inbound file *transfers* still need somewhere to land.

use std::path::PathBuf;

/// One file referenced on the clipboard.
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

impl LocalClip {
    /// Real twin's identity hash, kept here so the sync loop compiles
    /// unchanged. It is never reached in practice — [`ClipboardService::read`]
    /// returns an error on this build, so there is no clipboard content to
    /// fingerprint — but it is a pure function of the value and costs nothing
    /// to keep honest rather than stubbing it to a constant.
    #[allow(dead_code)]
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

/// Handle the mesh holds either way. Cheap to clone; owns nothing.
#[derive(Clone)]
pub struct ClipboardService {
    /// Never sent on — held only so [`ClipboardService::subscribe`] can hand
    /// back a receiver with the same type as the real service's. It parks the
    /// sync loop on a channel that stays silent for the process's life, which
    /// is exactly right: no OS clipboard here means nothing ever changes.
    changes: tokio::sync::broadcast::Sender<()>,
}

impl ClipboardService {
    /// No thread to spawn — there is no OS clipboard here to watch.
    pub fn spawn() -> ClipboardService {
        let (changes, _) = tokio::sync::broadcast::channel(1);
        ClipboardService { changes }
    }

    #[allow(dead_code)]
    pub fn read(&self) -> Result<Option<LocalClip>, String> {
        Err("OS clipboard is unavailable on this build".into())
    }

    pub fn file_paths(&self) -> Result<Vec<String>, String> {
        Err("OS clipboard is unavailable on this build".into())
    }

    /// A receiver that never fires. Keeping the sender alive on the struct is
    /// what makes that "silent" rather than "closed" — a closed channel would
    /// end the sync loop, and this build should simply have nothing to sync.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<()> {
        self.changes.subscribe()
    }

    pub fn set_text(&self, _text: String) -> Result<(), String> {
        Err("OS clipboard is unavailable on this build".into())
    }

    pub fn set_image(&self, _png: Vec<u8>) -> Result<(), String> {
        Err("OS clipboard is unavailable on this build".into())
    }

    pub fn set_files(&self, _paths: Vec<String>) -> Result<(), String> {
        Err("OS clipboard is unavailable on this build".into())
    }
}

pub fn local_files(_paths: Vec<String>) -> Result<Vec<LocalFile>, String> {
    Err("local file drops are unavailable on this build".into())
}

/// The staging directory a received clipboard transfer lands in. Per-transfer
/// so concurrent pastes never collide; under the system temp dir (iOS gives
/// every app its own).
pub fn staging_dir(transfer: u64) -> PathBuf {
    std::env::temp_dir()
        .join("allmystuff-clipboard")
        .join(transfer.to_string())
}
