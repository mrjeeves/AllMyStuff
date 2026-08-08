//! "Always On" window/startup behaviour — whether closing or minimizing the
//! main window keeps AllMyStuff alive in the tray / menu bar, and whether a
//! login-item launch starts hidden.
//!
//! The backend owns these preferences, not the front-end: the close / minimize
//! decision is made in a native window-event handler, and the start-hidden
//! decision in `setup`, both of which run whether or not the webview has
//! finished loading — so localStorage would be both too late and unreachable.
//! They're persisted next to the rest of AllMyStuff's state (under
//! `~/.myownmesh`, `MYOWNMESH_HOME`-overridable, exactly like `networks_store`)
//! and surfaced to the Svelte "Always On" tab through the `window_behavior` /
//! `set_window_behavior` commands. ("Start with computer" itself is an OS-level
//! login item owned by the autostart plugin, not stored here; only the
//! one-time "default it on" marker lives here.)

use std::path::PathBuf;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// The persisted preference. `#[serde(default)]` so an older file (or none)
/// still loads, and additive fields stay backward-compatible.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Behavior {
    /// Closing (the window's X) hides to the tray and keeps running, rather
    /// than quitting. Default on — the requested "close button minimizes".
    #[serde(default = "default_true")]
    pub close_to_tray: bool,
    /// Minimizing hides to the tray (gone from the taskbar), rather than the
    /// usual minimize. Default off — offered as a toggle.
    #[serde(default)]
    pub minimize_to_tray: bool,
    /// When launched by the login item (`--minimized`), start hidden in the
    /// tray instead of showing the window. Default off — offered as a toggle.
    #[serde(default)]
    pub start_minimized: bool,
    /// Internal: whether the one-time "Start with computer on by default" has
    /// been applied on this install. Not a user-facing preference — it just
    /// keeps us from re-enabling the login item after the user turns it off.
    #[serde(default)]
    pub autostart_defaulted: bool,
}

fn default_true() -> bool {
    true
}

impl Default for Behavior {
    fn default() -> Self {
        Behavior {
            close_to_tray: true,
            minimize_to_tray: false,
            start_minimized: false,
            autostart_defaulted: false,
        }
    }
}

/// The live store, cheap to share behind Tauri's managed state.
pub struct WindowBehavior {
    path: Option<PathBuf>,
    inner: Mutex<Behavior>,
}

impl WindowBehavior {
    /// Load the saved preference from disk (or start at the defaults).
    pub fn load() -> Self {
        let path = store_path();
        let inner = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str::<Behavior>(&s).ok())
            .unwrap_or_default();
        WindowBehavior {
            path,
            inner: Mutex::new(inner),
        }
    }

    /// The current preference (a cheap `Copy`).
    pub fn get(&self) -> Behavior {
        *self.inner.lock()
    }

    pub fn close_to_tray(&self) -> bool {
        self.inner.lock().close_to_tray
    }

    pub fn minimize_to_tray(&self) -> bool {
        self.inner.lock().minimize_to_tray
    }

    pub fn start_minimized(&self) -> bool {
        self.inner.lock().start_minimized
    }

    /// Whether the one-time "default Start with computer on" still needs
    /// applying (true until [`mark_autostart_defaulted`] records it).
    pub fn needs_autostart_default(&self) -> bool {
        !self.inner.lock().autostart_defaulted
    }

    /// Record that we've applied the default login item, and persist it, so we
    /// never re-enable it after the user has turned it off.
    pub fn mark_autostart_defaulted(&self) {
        let mut inner = self.inner.lock();
        inner.autostart_defaulted = true;
        persist(&self.path, &inner);
    }

    /// Update the preference and persist it. Returns the stored value (which
    /// equals the input when the write succeeded).
    pub fn set(&self, next: Behavior) -> Behavior {
        let mut inner = self.inner.lock();
        *inner = next;
        persist(&self.path, &inner);
        *inner
    }
}

fn persist(path: &Option<PathBuf>, value: &Behavior) -> bool {
    let Some(path) = path else {
        return false;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match serde_json::to_string_pretty(value) {
        Ok(s) => std::fs::write(path, s).is_ok(),
        Err(_) => false,
    }
}

/// `~/.myownmesh/allmystuff-window.json` (MYOWNMESH_HOME-overridable), beside
/// the rest of AllMyStuff's per-user state.
fn store_path() -> Option<PathBuf> {
    Some(allmystuff_protocol::myownmesh_state_dir()?.join("allmystuff-window.json"))
}
