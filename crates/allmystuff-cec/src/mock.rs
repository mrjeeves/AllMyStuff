//! An in-memory reference implementation of the CEC backend.
//!
//! [`MockBackend`] is the single source of behaviour for the whole contract:
//! tests drive it through [`MockTransport`] (no sockets), and the
//! `allmystuff-cec-mock` binary wraps the very same [`MockBackend::handle`]
//! in a tiny HTTP server so a human can run the real app against it.
//!
//! It is deliberately faithful — a fresh account has no entitlements, the
//! Ask-for-Help queue is only visible to *online* agents, a customer can only
//! see their own help sessions — with one concession to local life: it prints
//! sign-in codes (the start response carries a `dev_code` field, and tests
//! read [`MockBackend::last_code`]) instead of sending email, and it exposes
//! the mock-only `/v1/dev/grant` endpoint for setting up state.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{json, Value};

use crate::convention;
use crate::model::*;
use crate::transport::{ApiRequest, ApiResponse, Method, Transport};
use crate::Error;

/// Reference servers a mock CEC venue points at. Isolation between customers
/// comes from the unique `network_id` (the signaling room is derived from it),
/// not from the servers — so a working mock can share the public reference
/// venue and still hand every customer their own isolated mesh.
const REF_SIGNALING: &str = "wss://myownmesh.com";
const REF_STUN: &str = "stun:stun.myownmesh.com:3478";
const REF_TURN_URL: &str = "turn:turn.myownmesh.com:3478";
const REF_TURN_USER: &str = "guest";
const REF_TURN_PASS: &str = "theguestpassword";

/// Startup configuration for the reference backend.
#[derive(Debug, Clone, Default)]
pub struct MockConfig {
    /// Entitlements every freshly created account gets. Empty by default
    /// (faithful); the `--demo` mode of the server sets a generous default so
    /// the buttons light up.
    pub default_entitlements: Entitlements,
    /// If set, every account that signs in is also made an agent — handy for
    /// a one-laptop demo where the same person is customer and technician.
    pub everyone_is_agent: bool,
}

#[derive(Debug, Clone)]
struct AccountRec {
    account: Account,
    entitlements: Entitlements,
    private_lines: Vec<PrivateLine>,
    provision: Option<MeshProvision>,
}

#[derive(Default)]
struct State {
    next: u64,
    /// account_id → record
    accounts: HashMap<String, AccountRec>,
    /// email (lowercased) → account_id
    by_email: HashMap<String, String>,
    /// email → last code
    codes: HashMap<String, String>,
    /// bearer token → account_id
    tokens: HashMap<String, String>,
    /// help session id → session
    help: HashMap<String, HelpSession>,
    /// account_id → online-since (unix secs); absence means offline
    agents_online: HashMap<String, u64>,
    /// venue token → file (served at /v1/venues/{token})
    venues: HashMap<String, VenueFile>,
}

/// The in-memory CEC backend. Cheap to share via `Arc`.
pub struct MockBackend {
    cfg: MockConfig,
    state: Mutex<State>,
}

impl Default for MockBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl MockBackend {
    pub fn new() -> Self {
        Self::with_config(MockConfig::default())
    }

    pub fn with_config(cfg: MockConfig) -> Self {
        MockBackend {
            cfg,
            state: Mutex::new(State::default()),
        }
    }

    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    /// Read the last sign-in code emailed for `email` (tests use this in place
    /// of an inbox).
    pub fn last_code(&self, email: &str) -> Option<String> {
        self.state
            .lock()
            .unwrap()
            .codes
            .get(&email.to_lowercase())
            .cloned()
    }

    /// Route one request. This is the whole backend; the HTTP server is just a
    /// socket around it.
    pub fn handle(&self, req: ApiRequest) -> ApiResponse {
        let segs: Vec<&str> = req.path.trim_matches('/').split('/').collect();
        let mut st = self.state.lock().unwrap();
        match (req.method, segs.as_slice()) {
            // ---- auth ----------------------------------------------------
            (Method::Post, ["v1", "auth", "start"]) => self.auth_start(&mut st, &req),
            (Method::Post, ["v1", "auth", "verify"]) => self.auth_verify(&mut st, &req),
            (Method::Post, ["v1", "auth", "signout"]) => match account_for(&st, &req) {
                Ok(_) => {
                    if let Some(tok) = bearer(&req) {
                        st.tokens.remove(&tok);
                    }
                    ok(json!({ "ok": true }))
                }
                Err(e) => e,
            },
            (Method::Get, ["v1", "me"]) => match account_for(&st, &req) {
                Ok(id) => {
                    let rec = &st.accounts[&id];
                    ok(serde_json::to_value(Me {
                        account: rec.account.clone(),
                        entitlements: rec.entitlements.clone(),
                    })
                    .unwrap())
                }
                Err(e) => e,
            },
            (Method::Post, ["v1", "me", "device"]) => self.bind_device(&mut st, &req),

            // ---- the CEC mesh -------------------------------------------
            (Method::Post, ["v1", "mesh", "provision"]) => self.provision(&mut st, &req),
            (Method::Get, ["v1", "mesh"]) => match account_for(&st, &req) {
                Ok(id) => match &st.accounts[&id].provision {
                    Some(p) => ok(serde_json::to_value(p).unwrap()),
                    None => err(404, "no_mesh", "no CEC mesh provisioned"),
                },
                Err(e) => e,
            },

            // ---- Private Line -------------------------------------------
            (Method::Post, ["v1", "private-line"]) => self.rent_private_line(&mut st, &req),
            (Method::Get, ["v1", "private-line"]) => match account_for(&st, &req) {
                Ok(id) => ok(serde_json::to_value(&st.accounts[&id].private_lines).unwrap()),
                Err(e) => e,
            },
            (Method::Delete, ["v1", "private-line", pl_id]) => {
                self.cancel_private_line(&mut st, &req, pl_id)
            }

            // ---- venue files (served for remote venues) -----------------
            (Method::Get, ["v1", "venues", token]) => match st.venues.get(*token) {
                Some(file) => ok(serde_json::to_value(file).unwrap()),
                None => err(404, "no_venue", "unknown venue"),
            },

            // ---- Ask-for-Help (customer) --------------------------------
            (Method::Post, ["v1", "help"]) => self.ask_for_help(&mut st, &req),
            (Method::Get, ["v1", "help", id]) => self.help_status(&st, &req, id),
            (Method::Post, ["v1", "help", id, "cancel"]) => self.cancel_help(&mut st, &req, id),

            // ---- agent ---------------------------------------------------
            (Method::Post, ["v1", "agent", "presence"]) => self.agent_presence(&mut st, &req),
            (Method::Get, ["v1", "agent", "queue"]) => self.agent_queue(&st, &req),
            (Method::Post, ["v1", "agent", "help", id, "accept"]) => {
                self.agent_accept(&mut st, &req, id)
            }
            (Method::Post, ["v1", "agent", "help", id, "decline"]) => {
                self.agent_decline(&st, &req, id)
            }
            (Method::Post, ["v1", "agent", "help", id, "end"]) => self.agent_end(&mut st, &req, id),

            // ---- dev / mock-only ----------------------------------------
            (Method::Post, ["v1", "dev", "grant"]) => self.dev_grant(&mut st, &req),

            // ---- health --------------------------------------------------
            (Method::Get, ["v1", "health"]) | (Method::Get, []) => {
                ok(json!({ "ok": true, "service": "allmystuff-cec-mock" }))
            }

            _ => err(
                404,
                "not_found",
                &format!("no route for {} {}", req.method.as_str(), req.path),
            ),
        }
    }

    // --- auth --------------------------------------------------------------

    fn auth_start(&self, st: &mut State, req: &ApiRequest) -> ApiResponse {
        let body: StartSignIn = match parse(req) {
            Ok(b) => b,
            Err(e) => return e,
        };
        let email = body.email.trim().to_lowercase();
        if !email.contains('@') {
            return err(422, "bad_email", "that doesn't look like an email");
        }
        self.ensure_account(st, &email);
        let code = gen_code(&mut st.next);
        st.codes.insert(email.clone(), code.clone());
        // `dev_code` is mock-only: a real backend emails the code. Kept in the
        // body so the server can print it and a human can sign in locally; the
        // typed client ignores the unknown field.
        ok(json!({
            "sent": true,
            "masked_email": mask_email(&email),
            "dev_code": code,
        }))
    }

    fn auth_verify(&self, st: &mut State, req: &ApiRequest) -> ApiResponse {
        let body: VerifySignIn = match parse(req) {
            Ok(b) => b,
            Err(e) => return e,
        };
        let email = body.email.trim().to_lowercase();
        match st.codes.get(&email) {
            Some(expected) if *expected == body.code.trim() => {}
            _ => return err(401, "bad_code", "that code didn't match"),
        }
        st.codes.remove(&email);
        let id = self.ensure_account(st, &email);
        // Bind the device on the same call.
        if let Some(dev) = &body.device_id {
            bind(st.accounts.get_mut(&id).unwrap(), dev);
        }
        let token = gen_token(&mut st.next);
        st.tokens.insert(token.clone(), id.clone());
        let rec = &st.accounts[&id];
        ok(serde_json::to_value(Session {
            token,
            account: rec.account.clone(),
            entitlements: rec.entitlements.clone(),
        })
        .unwrap())
    }

    fn bind_device(&self, st: &mut State, req: &ApiRequest) -> ApiResponse {
        let id = match account_for(st, req) {
            Ok(id) => id,
            Err(e) => return e,
        };
        let body: BindDevice = match parse(req) {
            Ok(b) => b,
            Err(e) => return e,
        };
        let rec = st.accounts.get_mut(&id).unwrap();
        bind(rec, &body.device_id);
        ok(serde_json::to_value(&rec.account).unwrap())
    }

    // --- mesh --------------------------------------------------------------

    fn provision(&self, st: &mut State, req: &ApiRequest) -> ApiResponse {
        let id = match account_for(st, req) {
            Ok(id) => id,
            Err(e) => return e,
        };
        // Bind the asking device if provided.
        if let Ok(b) = parse::<Value>(req) {
            if let Some(dev) = b.get("device_id").and_then(|v| v.as_str()) {
                bind(st.accounts.get_mut(&id).unwrap(), dev);
            }
        }
        let p = ensure_provision(st, &id);
        ok(serde_json::to_value(p).unwrap())
    }

    // --- Private Line ------------------------------------------------------

    fn rent_private_line(&self, st: &mut State, req: &ApiRequest) -> ApiResponse {
        let id = match account_for(st, req) {
            Ok(id) => id,
            Err(e) => return e,
        };
        let body: RentPrivateLine = parse(req).unwrap_or_default();
        let pl_id = format!("pl_{}", next_id(&mut st.next));
        let token = format!("pl-{pl_id}");
        let label = body.label.unwrap_or_else(|| "Private Line".to_string());
        let spec = cec_venue_spec(&token, "private");
        st.venues
            .insert(token.clone(), VenueFile::new(label.clone(), &spec));
        let pl = PrivateLine {
            id: pl_id,
            label,
            status: SubscriptionStatus::Active,
            venue: spec,
            monthly_price_cents: 1000,
        };
        let rec = st.accounts.get_mut(&id).unwrap();
        rec.private_lines.push(pl.clone());
        rec.entitlements.private_line = true;
        ok(serde_json::to_value(pl).unwrap())
    }

    fn cancel_private_line(&self, st: &mut State, req: &ApiRequest, pl_id: &str) -> ApiResponse {
        let id = match account_for(st, req) {
            Ok(id) => id,
            Err(e) => return e,
        };
        let rec = st.accounts.get_mut(&id).unwrap();
        let mut found = false;
        for pl in rec.private_lines.iter_mut() {
            if pl.id == pl_id {
                pl.status = SubscriptionStatus::Cancelled;
                found = true;
            }
        }
        if !found {
            return err(404, "no_private_line", "no such Private Line");
        }
        rec.entitlements.private_line = rec
            .private_lines
            .iter()
            .any(|pl| pl.status == SubscriptionStatus::Active);
        ok(json!({ "ok": true }))
    }

    // --- help (customer) ---------------------------------------------------

    fn ask_for_help(&self, st: &mut State, req: &ApiRequest) -> ApiResponse {
        let id = match account_for(st, req) {
            Ok(id) => id,
            Err(e) => return e,
        };
        let body: AskForHelp = match parse(req) {
            Ok(b) => b,
            Err(e) => return e,
        };
        let p = ensure_provision(st, &id);
        let rec = &st.accounts[&id];
        let session = HelpSession {
            id: format!("help_{}", next_id(&mut st.next)),
            status: HelpStatus::Queued,
            network_id: body.network_id,
            room_id: body.room_id,
            cec_service_node_id: p.cec_service_node_id.clone(),
            customer_device_id: body.device_id,
            customer_label: display_label(&rec.account),
            topic: body.topic,
            agent_label: None,
            created_at: now_secs(),
        };
        st.help.insert(session.id.clone(), session.clone());
        ok(serde_json::to_value(session).unwrap())
    }

    fn help_status(&self, st: &State, req: &ApiRequest, id: &str) -> ApiResponse {
        let acct = match account_for(st, req) {
            Ok(a) => a,
            Err(e) => return e,
        };
        match st.help.get(id) {
            Some(s) => {
                let owns = st.accounts[&acct]
                    .account
                    .device_ids
                    .contains(&s.customer_device_id);
                if owns || st.accounts[&acct].account.is_agent() {
                    ok(serde_json::to_value(s).unwrap())
                } else {
                    err(403, "forbidden", "not your help session")
                }
            }
            None => err(404, "no_help", "no such help session"),
        }
    }

    fn cancel_help(&self, st: &mut State, req: &ApiRequest, id: &str) -> ApiResponse {
        if let Err(e) = account_for(st, req) {
            return e;
        }
        match st.help.get_mut(id) {
            Some(s) if !s.status.is_terminal() => {
                s.status = HelpStatus::Cancelled;
                ok(json!({ "ok": true }))
            }
            Some(_) => err(409, "already_done", "session already finished"),
            None => err(404, "no_help", "no such help session"),
        }
    }

    // --- agent -------------------------------------------------------------

    fn agent_presence(&self, st: &mut State, req: &ApiRequest) -> ApiResponse {
        let id = match agent_for(st, req) {
            Ok(id) => id,
            Err(e) => return e,
        };
        let body: SetPresence = match parse(req) {
            Ok(b) => b,
            Err(e) => return e,
        };
        let since = if body.online {
            let s = now_secs();
            st.agents_online.insert(id.clone(), s);
            s
        } else {
            st.agents_online.remove(&id);
            0
        };
        ok(serde_json::to_value(AgentPresence {
            online: body.online,
            since,
        })
        .unwrap())
    }

    fn agent_queue(&self, st: &State, req: &ApiRequest) -> ApiResponse {
        let id = match agent_for(st, req) {
            Ok(id) => id,
            Err(e) => return e,
        };
        if !st.agents_online.contains_key(&id) {
            return err(409, "offline", "go online to see the queue");
        }
        let mut queued: Vec<HelpSession> = st
            .help
            .values()
            .filter(|s| s.status == HelpStatus::Queued)
            .cloned()
            .collect();
        queued.sort_by_key(|s| s.created_at);
        ok(serde_json::to_value(queued).unwrap())
    }

    fn agent_accept(&self, st: &mut State, req: &ApiRequest, id: &str) -> ApiResponse {
        let agent = match agent_for(st, req) {
            Ok(a) => a,
            Err(e) => return e,
        };
        if !st.agents_online.contains_key(&agent) {
            return err(409, "offline", "go online before accepting");
        }
        let label = display_label(&st.accounts[&agent].account);
        let session = match st.help.get_mut(id) {
            Some(s) if s.status == HelpStatus::Queued => {
                s.status = HelpStatus::Assigned;
                s.agent_label = Some(label);
                s.clone()
            }
            Some(_) => return err(409, "taken", "that session is no longer waiting"),
            None => return err(404, "no_help", "no such help session"),
        };
        // The venue is whatever serves the customer's CEC network. Re-derive
        // from the venue token convention so we don't have to store a back-ref.
        let venue = cec_venue_spec(&cec_mesh_venue_token(&session.network_id), "service");
        ok(serde_json::to_value(AgentAssignment { session, venue }).unwrap())
    }

    fn agent_decline(&self, st: &State, req: &ApiRequest, id: &str) -> ApiResponse {
        if let Err(e) = agent_for(st, req) {
            return e;
        }
        // Decline just leaves it queued for another agent.
        if st.help.contains_key(id) {
            ok(json!({ "ok": true }))
        } else {
            err(404, "no_help", "no such help session")
        }
    }

    fn agent_end(&self, st: &mut State, req: &ApiRequest, id: &str) -> ApiResponse {
        if let Err(e) = agent_for(st, req) {
            return e;
        }
        match st.help.get_mut(id) {
            Some(s) => {
                s.status = HelpStatus::Ended;
                ok(json!({ "ok": true }))
            }
            None => err(404, "no_help", "no such help session"),
        }
    }

    // --- dev ---------------------------------------------------------------

    fn dev_grant(&self, st: &mut State, req: &ApiRequest) -> ApiResponse {
        let body: DevGrant = match parse(req) {
            Ok(b) => b,
            Err(e) => return e,
        };
        let email = body.email.trim().to_lowercase();
        if !email.contains('@') {
            return err(422, "bad_email", "that doesn't look like an email");
        }
        let id = self.ensure_account(st, &email);
        let rec = st.accounts.get_mut(&id).unwrap();
        if let Some(ent) = body.entitlements {
            rec.entitlements = ent;
        }
        if body.agent && !rec.account.roles.contains(&AccountRole::Agent) {
            rec.account.roles.push(AccountRole::Agent);
        }
        ok(json!({ "ok": true }))
    }

    // --- helpers -----------------------------------------------------------

    /// Find-or-create an account by email, applying the configured defaults.
    fn ensure_account(&self, st: &mut State, email: &str) -> String {
        if let Some(id) = st.by_email.get(email) {
            return id.clone();
        }
        let id = format!("acct_{}", next_id(&mut st.next));
        let mut roles = vec![AccountRole::Customer];
        if self.cfg.everyone_is_agent {
            roles.push(AccountRole::Agent);
        }
        let account = Account {
            id: id.clone(),
            email: email.to_string(),
            display_name: email.split('@').next().unwrap_or(email).to_string(),
            roles,
            device_ids: Vec::new(),
        };
        st.accounts.insert(
            id.clone(),
            AccountRec {
                account,
                entitlements: self.cfg.default_entitlements.clone(),
                private_lines: Vec::new(),
                provision: None,
            },
        );
        st.by_email.insert(email.to_string(), id.clone());
        id
    }
}

/// Find-or-create the CEC mesh provision for an account, registering its
/// venue file so the app's remote-venue fetch resolves it.
fn ensure_provision(st: &mut State, account_id: &str) -> MeshProvision {
    if let Some(p) = &st.accounts[account_id].provision {
        return p.clone();
    }
    let network_id = convention::customer_network_id(account_id);
    let cec_service_node_id = format!("cecservice{}", convention::short_hash(account_id));
    let token = cec_mesh_venue_token(&network_id);
    let spec = cec_venue_spec(&token, "service");
    st.venues
        .insert(token, VenueFile::new(convention::CEC_NETWORK_LABEL, &spec));
    let provision = MeshProvision {
        network_id,
        label: convention::CEC_NETWORK_LABEL.to_string(),
        venue: spec,
        cec_service_node_id,
        auto_approve: true,
    };
    st.accounts.get_mut(account_id).unwrap().provision = Some(provision.clone());
    provision
}

fn cec_mesh_venue_token(network_id: &str) -> String {
    format!("mesh-{network_id}")
}

/// A CEC venue spec: a live venue-file URL plus inline reference servers.
fn cec_venue_spec(token: &str, _kind: &str) -> VenueSpec {
    VenueSpec {
        url: Some(format!("/v1/venues/{token}")),
        signaling: vec![REF_SIGNALING.into()],
        stun: vec![REF_STUN.into()],
        turn: vec![TurnCredential {
            url: REF_TURN_URL.into(),
            username: REF_TURN_USER.into(),
            credential: REF_TURN_PASS.into(),
        }],
    }
}

fn bind(rec: &mut AccountRec, device_id: &str) {
    if !device_id.is_empty() && !rec.account.device_ids.iter().any(|d| d == device_id) {
        rec.account.device_ids.push(device_id.to_string());
    }
}

fn display_label(a: &Account) -> String {
    if a.display_name.is_empty() {
        a.email.split('@').next().unwrap_or(&a.email).to_string()
    } else {
        a.display_name.clone()
    }
}

/// Resolve the bearer token to an account id, or an auth error.
fn account_for(st: &State, req: &ApiRequest) -> Result<String, ApiResponse> {
    let token = bearer(req).ok_or_else(|| err(401, "no_token", "sign in first"))?;
    st.tokens
        .get(&token)
        .cloned()
        .ok_or_else(|| err(401, "bad_token", "session expired"))
}

/// Like [`account_for`] but also requires the agent role.
fn agent_for(st: &State, req: &ApiRequest) -> Result<String, ApiResponse> {
    let id = account_for(st, req)?;
    if st.accounts[&id].account.is_agent() {
        Ok(id)
    } else {
        Err(err(403, "not_agent", "this account isn't a CEC agent"))
    }
}

fn bearer(req: &ApiRequest) -> Option<String> {
    req.bearer.clone()
}

fn parse<T: serde::de::DeserializeOwned>(req: &ApiRequest) -> Result<T, ApiResponse> {
    let body = req.body.clone().unwrap_or(Value::Null);
    serde_json::from_value(body).map_err(|e| err(422, "bad_body", &e.to_string()))
}

fn ok(body: Value) -> ApiResponse {
    ApiResponse::ok(body)
}

fn err(status: u16, code: &str, message: &str) -> ApiResponse {
    ApiResponse::new(
        status,
        json!({ "error": { "code": code, "message": message } }),
    )
}

fn next_id(next: &mut u64) -> u64 {
    *next += 1;
    *next
}

fn gen_token(next: &mut u64) -> String {
    let n = next_id(next);
    format!("tok_{n:06}_{}", convention::new_nonce())
}

fn gen_code(next: &mut u64) -> String {
    // A 6-digit code, varied by a global counter so concurrent tests differ.
    static SALT: AtomicU64 = AtomicU64::new(0);
    let n = next_id(next);
    let salt = SALT.fetch_add(1, Ordering::Relaxed);
    let v = (now_secs()
        .wrapping_add(n)
        .wrapping_add(salt.wrapping_mul(7)))
        % 1_000_000;
    format!("{v:06}")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn mask_email(email: &str) -> String {
    match email.split_once('@') {
        Some((local, domain)) => {
            let shown = local.chars().next().unwrap_or('•');
            format!("{shown}•••@{domain}")
        }
        None => "•••".into(),
    }
}

/// A [`Transport`] that calls a shared [`MockBackend`] directly — no sockets,
/// no async I/O, perfect for tests and for an embedded demo backend.
#[derive(Clone)]
pub struct MockTransport {
    backend: Arc<MockBackend>,
}

impl MockTransport {
    pub fn new(backend: Arc<MockBackend>) -> Self {
        MockTransport { backend }
    }

    /// A fresh mock + transport in one go.
    pub fn fresh() -> (Arc<MockBackend>, Self) {
        let backend = MockBackend::shared();
        let transport = MockTransport::new(backend.clone());
        (backend, transport)
    }

    pub fn backend(&self) -> &Arc<MockBackend> {
        &self.backend
    }
}

impl Transport for MockTransport {
    async fn send(&self, req: ApiRequest) -> Result<ApiResponse, Error> {
        // Synchronous, in-memory — the lock is never held across an await
        // (there is no await), so the future is trivially `Send`.
        Ok(self.backend.handle(req))
    }
}

/// Convenience: serialise a value to a JSON body for ad-hoc requests.
pub fn body<T: Serialize>(v: &T) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}
