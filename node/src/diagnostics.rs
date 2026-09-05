//! Local, opt-in development diagnostics.
//!
//! The desktop setting and the headless node share this tiny preference file
//! under the ordinary AllMyStuff state root. Nothing crosses the mesh or its
//! signaling layer. Environment variables remain the highest-priority escape
//! hatch for a one-shot diagnostic launch.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Route child/frontend diagnostics through the node's file-backed tracing
/// layers. Bound Unicode safely and escape record separators via Debug fields.
pub(crate) fn record_frontend_line(line: &str) {
    let line: String = line.chars().take(4096).collect();
    tracing::info!(target: "allmystuff_node::frontend", ?line, "frontend diagnostic");
}

pub(crate) fn record_daemon_line(line: &str, stderr: bool) {
    let line: String = line.chars().take(4096).collect();
    tracing::debug!(target: "allmystuff_node::daemon", stderr, ?line, "myownmesh diagnostic");
}

#[derive(Default, Deserialize, Serialize)]
struct Preferences {
    /// `Option` preserves the distinction between an untouched install and a
    /// user who explicitly turned an instrumented build back off.
    #[serde(default)]
    debug_logging: Option<bool>,
}

/// The effective verbose-file setting. The explicit environment dial wins,
/// followed by the persisted in-app toggle. Every build defaults off: even a
/// binary compiled with field telemetry must be explicitly opted into verbose
/// file logging at runtime.
pub fn debug_logging_enabled() -> bool {
    let env = std::env::var("ALLMYSTUFF_CWD_LOG").ok();
    let stored = store_path()
        .as_deref()
        .map(crate::persist::load_json::<Preferences>)
        .and_then(|prefs| prefs.debug_logging);
    resolve_debug_logging(env.as_deref(), stored)
}

/// Persist the in-app development toggle. The node reads this during logging
/// initialization, so a running backend picks it up on its next restart.
pub fn set_debug_logging(enabled: bool) -> std::io::Result<()> {
    let path = store_path().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "could not resolve the AllMyStuff settings directory",
        )
    })?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_vec_pretty(&Preferences {
        debug_logging: Some(enabled),
    })
    .map_err(std::io::Error::other)?;
    crate::persist::write_atomic(&path, &json)
}

fn resolve_debug_logging(env: Option<&str>, stored: Option<bool>) -> bool {
    match env {
        Some(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "off" | "false"
        ),
        None => stored.unwrap_or(false),
    }
}

/// `~/.myownmesh/allmystuff-diagnostics.json`, honoring `MYOWNMESH_HOME` in
/// the same way as the node socket and the other local stores.
fn store_path() -> Option<PathBuf> {
    Some(allmystuff_protocol::myownmesh_state_dir()?.join("allmystuff-diagnostics.json"))
}

#[cfg(test)]
mod tests {
    use super::resolve_debug_logging;

    #[test]
    fn forwarded_diagnostics_reach_the_tracing_writer_bounded_and_escaped() {
        use std::io::Write;
        use std::sync::{Arc, Mutex};
        #[derive(Clone)]
        struct Capture(Arc<Mutex<Vec<u8>>>);
        impl Write for Capture {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let captured = Capture(Arc::new(Mutex::new(Vec::new())));
        let sink = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .without_time()
            .with_writer(move || sink.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            super::record_frontend_line("video stages\nnot a second record");
            super::record_daemon_line("video loss before daemon IPC handoff", false);
            super::record_frontend_line(&format!("{}MUST_BE_TRUNCATED", "é".repeat(4096)));
        });
        let text = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        assert_eq!(text.lines().count(), 3);
        assert!(text.contains("video stages\\nnot a second record"));
        assert!(text.contains("video loss before daemon IPC handoff"));
        assert!(!text.contains("MUST_BE_TRUNCATED"));
    }

    #[test]
    fn debug_logging_is_opt_in_and_explicit_off_wins() {
        assert!(!resolve_debug_logging(None, None));
        assert!(resolve_debug_logging(None, Some(true)));
        assert!(!resolve_debug_logging(None, Some(false)));
        assert!(resolve_debug_logging(Some("1"), Some(false)));
        assert!(!resolve_debug_logging(Some("off"), Some(true)));
    }
}
