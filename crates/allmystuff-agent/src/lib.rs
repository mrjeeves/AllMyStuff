//! The reusable core of the CEC agent tool: persisted config plus the
//! transport-generic "watch" operation, kept out of `main.rs` so it can be
//! tested against the in-memory backend without a socket.

use std::path::{Path, PathBuf};

use allmystuff_cec::model::{AgentAssignment, HelpSession};
use allmystuff_cec::{CecClient, Error, Transport};
use serde::{Deserialize, Serialize};

/// Where the app family advertises its reference backend. Override with
/// `--backend` (e.g. point at a local `allmystuff-cec-mock`).
pub const DEFAULT_BACKEND: &str = "https://api.allmystuff.works";

/// The agent's persisted state: which backend, the session token, and a note
/// of who's signed in. Lives next to the mesh identity under `~/.myownmesh`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_backend")]
    pub backend_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

fn default_backend() -> String {
    DEFAULT_BACKEND.to_string()
}

impl Default for Config {
    fn default() -> Self {
        Config {
            backend_url: default_backend(),
            token: None,
            email: None,
        }
    }
}

impl Config {
    /// `~/.myownmesh/allmystuff-agent.json`, honouring `MYOWNMESH_HOME` exactly
    /// like the daemon control socket does.
    pub fn default_path() -> Option<PathBuf> {
        let home = std::env::var_os("MYOWNMESH_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))?;
        Some(home.join(".myownmesh").join("allmystuff-agent.json"))
    }

    /// Load config from `path`, or a default if it doesn't exist.
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Persist to `path`, creating the parent directory.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = serde_json::to_string_pretty(self).unwrap_or_default();
        std::fs::write(path, body)
    }

    pub fn is_signed_in(&self) -> bool {
        self.token.is_some()
    }
}

/// What one pass of `watch` found (and possibly took).
#[derive(Debug, Clone)]
pub struct WatchReport {
    pub queued: Vec<HelpSession>,
    pub accepted: Option<AgentAssignment>,
}

/// One pass of the agent loop: read the queue, and — when `accept_all` — take
/// the oldest waiting session, returning the assignment to act on.
///
/// The actual live link to the customer is the backend's job: it operates the
/// single CEC Service node on the customer's mesh and bridges this agent to
/// it. The agent tool's responsibility ends at "I've got it" — exactly the
/// "connections are managed by the backend provider, not the mesh engine"
/// split the product describes.
pub async fn watch_once<T: Transport>(
    client: &CecClient<T>,
    accept_all: bool,
) -> Result<WatchReport, Error> {
    let queued = client.agent_queue().await?;
    let mut accepted = None;
    if accept_all {
        if let Some(first) = queued.iter().min_by_key(|s| s.created_at) {
            accepted = Some(client.accept_help(&first.id).await?);
        }
    }
    Ok(WatchReport { queued, accepted })
}

/// A one-line summary of a help session for the console.
pub fn fmt_session(s: &HelpSession) -> String {
    let topic = s.topic.as_deref().unwrap_or("(no topic)");
    format!(
        "  {} · {} · {} · {}",
        s.id,
        s.customer_label,
        status_word(s),
        topic
    )
}

fn status_word(s: &HelpSession) -> &'static str {
    use allmystuff_cec::model::HelpStatus::*;
    match s.status {
        Queued => "waiting",
        Assigned => "assigned",
        Connected => "connected",
        Ended => "ended",
        Cancelled => "cancelled",
    }
}
