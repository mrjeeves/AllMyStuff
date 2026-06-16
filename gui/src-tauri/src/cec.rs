//! The CEC service, app-backend side.
//!
//! This is the Tauri half of the [`allmystuff-cec`] client: it owns the
//! persisted account state (`~/.myownmesh/allmystuff-cec.json` — the same home
//! as the rest of AllMyStuff's state) and a `reqwest`-backed client, and
//! exposes the small set of operations the front-end drives through Tauri
//! commands: set the backend URL, sign in with an email code (binding this
//! device's mesh identity), provision the customer's CEC mesh, rent a Private
//! Line, and open / poll an Ask-for-Help session.
//!
//! The actual mesh work — joining the `cec-customer-<hash>` network, approving
//! the CEC Service node, minting the help room — is orchestrated in the
//! front-end store on top of the existing network/venue/room commands; this
//! module just talks to the backend and remembers what it learned.

use std::path::PathBuf;

use allmystuff_cec::model::{Account, Entitlements, MeshProvision, VenueSpec};
use allmystuff_cec::{AskForHelp, CecClient, Error as CecError, ReqwestTransport};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// The reference backend the app talks to by default; overridable in
/// Settings → Account (point it at a local `allmystuff-cec-mock` to try the
/// whole flow offline).
const DEFAULT_BACKEND: &str = "https://api.allmystuff.works";

/// On-disk state. Additive + `#[serde(default)]` so an older file (or none)
/// still loads, exactly like `networks_store`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Persisted {
    #[serde(default = "default_backend")]
    backend_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account: Option<Account>,
    #[serde(default)]
    entitlements: Entitlements,
    /// The last CEC mesh descriptor the backend handed us, so the graph can
    /// label the network and its single service node even before a refresh.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provision: Option<MeshProvision>,
}

impl Default for Persisted {
    fn default() -> Self {
        Persisted {
            backend_url: default_backend(),
            token: None,
            account: None,
            entitlements: Entitlements::default(),
            provision: None,
        }
    }
}

fn default_backend() -> String {
    DEFAULT_BACKEND.to_string()
}

/// The live CEC state behind the Tauri `State`. Cheap to share.
pub struct Cec {
    path: Option<PathBuf>,
    inner: Mutex<Persisted>,
}

impl Cec {
    /// Load persisted state from disk (or start signed-out with the default
    /// backend).
    pub fn load() -> Self {
        let path = store_path();
        let inner = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str::<Persisted>(&s).ok())
            .unwrap_or_default();
        Cec {
            path,
            inner: Mutex::new(inner),
        }
    }

    // ---- snapshot for the UI ---------------------------------------------

    /// The `{ backend_url, signed_in, account, entitlements, provision }` blob
    /// the front-end renders from. Never includes the bearer token.
    pub fn snapshot(&self) -> Value {
        let st = self.inner.lock();
        snapshot_of(&st)
    }

    // ---- backend URL ------------------------------------------------------

    pub fn set_backend_url(&self, url: String) -> Result<Value, String> {
        let url = url.trim().trim_end_matches('/').to_string();
        if url.is_empty() {
            return Err("backend URL can't be empty".into());
        }
        // Changing the backend invalidates any session against the old one.
        {
            let mut st = self.inner.lock();
            if st.backend_url != url {
                st.backend_url = url;
                st.token = None;
                st.account = None;
                st.entitlements = Entitlements::default();
                st.provision = None;
            }
            persist(&self.path, &st);
        }
        Ok(self.snapshot())
    }

    // ---- auth -------------------------------------------------------------

    pub async fn start_sign_in(&self, email: String) -> Result<Value, String> {
        let client = self.client();
        let resp = client.start_sign_in(&email).await.map_err(describe)?;
        Ok(json!({
            "sent": resp.sent,
            "masked_email": resp.masked_email.unwrap_or(email),
        }))
    }

    pub async fn verify_sign_in(
        &self,
        email: String,
        code: String,
        device_id: Option<String>,
        device_label: Option<String>,
    ) -> Result<Value, String> {
        let mut client = self.client();
        let session = client
            .verify_sign_in(&email, &code, device_id.as_deref(), device_label.as_deref())
            .await
            .map_err(describe)?;
        let mut st = self.inner.lock();
        st.token = Some(session.token);
        st.account = Some(session.account);
        st.entitlements = session.entitlements;
        persist(&self.path, &st);
        Ok(snapshot_of(&st))
    }

    /// Re-fetch the account + entitlements (e.g. after a purchase on the web).
    pub async fn refresh(&self) -> Result<Value, String> {
        let client = self.client();
        match client.me().await {
            Ok(me) => {
                let mut st = self.inner.lock();
                st.account = Some(me.account);
                st.entitlements = me.entitlements;
                persist(&self.path, &st);
                Ok(snapshot_of(&st))
            }
            // An expired/invalid session signs the user out locally rather
            // than wedging the UI in a half-signed-in state.
            Err(e) if e.is_auth() => {
                let mut st = self.inner.lock();
                clear_session(&mut st);
                persist(&self.path, &st);
                Ok(snapshot_of(&st))
            }
            Err(e) => Err(describe(e)),
        }
    }

    pub async fn sign_out(&self) -> Result<Value, String> {
        let mut client = self.client();
        let _ = client.sign_out().await; // best-effort
        let mut st = self.inner.lock();
        clear_session(&mut st);
        persist(&self.path, &st);
        Ok(snapshot_of(&st))
    }

    // ---- the CEC mesh -----------------------------------------------------

    /// Provision (or fetch) the customer's CEC mesh descriptor. The venue URL
    /// is absolutised against the backend so the front-end can add it as an
    /// ordinary remote venue and fetch it directly.
    pub async fn provision_mesh(&self, device_id: String) -> Result<Value, String> {
        let client = self.client();
        let mut prov = client.provision_mesh(&device_id).await.map_err(describe)?;
        let base = self.backend_url();
        absolutize_venue(&base, &mut prov.venue);
        {
            let mut st = self.inner.lock();
            st.provision = Some(prov.clone());
            persist(&self.path, &st);
        }
        serde_json::to_value(prov).map_err(|e| e.to_string())
    }

    // ---- Private Line -----------------------------------------------------

    pub async fn rent_private_line(&self, label: Option<String>) -> Result<Value, String> {
        let client = self.client();
        let mut pl = client
            .rent_private_line(label.as_deref())
            .await
            .map_err(describe)?;
        absolutize_venue(&self.backend_url(), &mut pl.venue);
        // A new subscription flips the entitlement; reflect it without waiting
        // for the next refresh.
        {
            let mut st = self.inner.lock();
            st.entitlements.private_line = true;
            persist(&self.path, &st);
        }
        serde_json::to_value(pl).map_err(|e| e.to_string())
    }

    pub async fn list_private_lines(&self) -> Result<Value, String> {
        let client = self.client();
        let mut lines = client.list_private_lines().await.map_err(describe)?;
        let base = self.backend_url();
        for pl in &mut lines {
            absolutize_venue(&base, &mut pl.venue);
        }
        serde_json::to_value(lines).map_err(|e| e.to_string())
    }

    pub async fn cancel_private_line(&self, id: String) -> Result<Value, String> {
        let client = self.client();
        client.cancel_private_line(&id).await.map_err(describe)?;
        Ok(self.snapshot())
    }

    // ---- Ask-for-Help -----------------------------------------------------

    pub async fn ask_for_help(
        &self,
        network_id: String,
        room_id: String,
        device_id: String,
        topic: Option<String>,
    ) -> Result<Value, String> {
        let client = self.client();
        let session = client
            .ask_for_help(&AskForHelp {
                network_id,
                room_id,
                device_id,
                topic,
            })
            .await
            .map_err(describe)?;
        serde_json::to_value(session).map_err(|e| e.to_string())
    }

    pub async fn help_status(&self, id: String) -> Result<Value, String> {
        let client = self.client();
        let session = client.help_status(&id).await.map_err(describe)?;
        serde_json::to_value(session).map_err(|e| e.to_string())
    }

    pub async fn cancel_help(&self, id: String) -> Result<Value, String> {
        let client = self.client();
        client.cancel_help(&id).await.map_err(describe)?;
        Ok(json!({ "ok": true }))
    }

    // ---- helpers ----------------------------------------------------------

    fn backend_url(&self) -> String {
        self.inner.lock().backend_url.clone()
    }

    /// Build a client for the current backend + token. Cheap enough for these
    /// user-driven, low-frequency calls; the lock is released before any
    /// `await`, so this never blocks the runtime.
    fn client(&self) -> CecClient<ReqwestTransport> {
        let (url, token) = {
            let st = self.inner.lock();
            (st.backend_url.clone(), st.token.clone())
        };
        // `ReqwestTransport::new` only fails building the HTTP client, which is
        // effectively infallible here; fall back to the default backend so a
        // method still returns a real (transport) error rather than panicking.
        let transport = ReqwestTransport::new(&url)
            .or_else(|_| ReqwestTransport::new(DEFAULT_BACKEND))
            .expect("build reqwest client");
        CecClient::with_token(transport, token)
    }
}

fn clear_session(st: &mut Persisted) {
    st.token = None;
    st.account = None;
    st.entitlements = Entitlements::default();
    st.provision = None;
}

fn snapshot_of(st: &Persisted) -> Value {
    json!({
        "backend_url": st.backend_url,
        "signed_in": st.token.is_some(),
        "account": st.account,
        "entitlements": st.entitlements,
        "provision": st.provision,
    })
}

/// Turn a relative venue URL (`/v1/venues/…`) into an absolute one against the
/// backend, so the front-end (and the daemon, indirectly) can fetch it.
fn absolutize_venue(base: &str, venue: &mut VenueSpec) {
    // Take the url out before reassigning so no borrow of `venue.url` is alive
    // across the mutation (and it reads cleanly for clippy).
    let needs_abs = venue.url.as_deref().is_some_and(|u| u.starts_with('/'));
    if needs_abs {
        let url = venue.url.take().unwrap_or_default();
        venue.url = Some(format!("{}{}", base.trim_end_matches('/'), url));
    }
}

fn describe(e: CecError) -> String {
    match &e {
        CecError::Api {
            code: Some(c),
            message,
            ..
        } => format!("{message} ({c})"),
        _ => e.to_string(),
    }
}

fn persist(path: &Option<PathBuf>, value: &Persisted) -> bool {
    let Some(path) = path else {
        return false;
    };
    let Ok(json) = serde_json::to_string_pretty(value) else {
        return false;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(path, json).is_ok()
}

/// `~/.myownmesh/allmystuff-cec.json`, honouring `MYOWNMESH_HOME` — the same
/// home as the mesh identity and the rest of AllMyStuff's state.
fn store_path() -> Option<PathBuf> {
    let home = std::env::var_os("MYOWNMESH_HOME")
        .map(PathBuf::from)
        .or_else(dirs::home_dir)?;
    Some(home.join(".myownmesh").join("allmystuff-cec.json"))
}
