//! The CEC service client — one typed method per endpoint in the contract.
//!
//! Generic over a [`Transport`], so production code holds a
//! `CecClient<ReqwestTransport>` and tests hold a `CecClient<MockTransport>`
//! over the same logic. The bearer token, once obtained from
//! [`verify_sign_in`](CecClient::verify_sign_in), is carried on every
//! authenticated call.

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::model::*;
use crate::transport::{ApiRequest, Method, Transport};
use crate::Error;

/// A client for the CEC backend. Cheap to clone if the transport is.
pub struct CecClient<T: Transport> {
    transport: T,
    token: Option<String>,
}

impl<T: Transport> CecClient<T> {
    /// Build a client over `transport`, signed out.
    pub fn new(transport: T) -> Self {
        CecClient {
            transport,
            token: None,
        }
    }

    /// Build a client over `transport` that's already holding a session token
    /// (e.g. one restored from disk).
    pub fn with_token(transport: T, token: Option<String>) -> Self {
        CecClient { transport, token }
    }

    /// The current bearer token, if signed in.
    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    pub fn is_signed_in(&self) -> bool {
        self.token.is_some()
    }

    /// Set (or replace) the session token.
    pub fn set_token(&mut self, token: Option<String>) {
        self.token = token;
    }

    /// Forget the session token (local sign-out).
    pub fn clear_token(&mut self) {
        self.token = None;
    }

    /// Borrow the underlying transport (e.g. to read a base URL).
    pub fn transport(&self) -> &T {
        &self.transport
    }

    // --- low-level plumbing ------------------------------------------------

    /// Send `req`, attaching the held bearer token unless one is already set,
    /// then map the response: 2xx → decode `T2`, otherwise → [`Error::Api`].
    async fn call<R: DeserializeOwned>(&self, mut req: ApiRequest) -> Result<R, Error> {
        if req.bearer.is_none() {
            req.bearer = self.token.clone();
        }
        let resp = self.transport.send(req).await?;
        if resp.is_success() {
            // An empty 2xx body decodes to `R` only when `R` allows it (e.g.
            // unit or an all-optional struct); otherwise it's a decode error,
            // which is the honest outcome.
            serde_json::from_value(resp.body).map_err(|e| Error::Decode(e.to_string()))
        } else {
            Err(api_error(resp.status, resp.body))
        }
    }

    /// Like [`call`](Self::call) but for endpoints with no useful body.
    async fn call_unit(&self, mut req: ApiRequest) -> Result<(), Error> {
        if req.bearer.is_none() {
            req.bearer = self.token.clone();
        }
        let resp = self.transport.send(req).await?;
        if resp.is_success() {
            Ok(())
        } else {
            Err(api_error(resp.status, resp.body))
        }
    }

    fn require_token(&self) -> Result<(), Error> {
        if self.token.is_some() {
            Ok(())
        } else {
            Err(Error::Unauthenticated)
        }
    }

    // --- auth --------------------------------------------------------------

    /// `POST /v1/auth/start` — email a one-time sign-in code.
    pub async fn start_sign_in(&self, email: &str) -> Result<StartSignInResponse, Error> {
        let body = serde_json::to_value(StartSignIn {
            email: email.to_string(),
        })
        .map_err(enc)?;
        self.call(ApiRequest::post("/v1/auth/start").body(body)).await
    }

    /// `POST /v1/auth/verify` — exchange the code for a session and (if given)
    /// bind this device's mesh identity. On success the returned session's
    /// token is **stored on the client** for subsequent calls.
    pub async fn verify_sign_in(
        &mut self,
        email: &str,
        code: &str,
        device_id: Option<&str>,
        device_label: Option<&str>,
    ) -> Result<Session, Error> {
        let body = serde_json::to_value(VerifySignIn {
            email: email.to_string(),
            code: code.to_string(),
            device_id: device_id.map(str::to_string),
            device_label: device_label.map(str::to_string),
        })
        .map_err(enc)?;
        let session: Session = self.call(ApiRequest::post("/v1/auth/verify").body(body)).await?;
        self.token = Some(session.token.clone());
        Ok(session)
    }

    /// `GET /v1/me` — the current account + entitlements.
    pub async fn me(&self) -> Result<Me, Error> {
        self.require_token()?;
        self.call(ApiRequest::get("/v1/me")).await
    }

    /// `POST /v1/auth/signout` — invalidate the session server-side, then drop
    /// the local token.
    pub async fn sign_out(&mut self) -> Result<(), Error> {
        if self.token.is_some() {
            // Best-effort: a network blip shouldn't trap the user signed in.
            let _ = self.call_unit(ApiRequest::post("/v1/auth/signout")).await;
        }
        self.token = None;
        Ok(())
    }

    /// `POST /v1/me/device` — bind (or re-label) a mesh device on the account.
    pub async fn bind_device(&self, device_id: &str, label: Option<&str>) -> Result<Account, Error> {
        self.require_token()?;
        let body = serde_json::to_value(BindDevice {
            device_id: device_id.to_string(),
            label: label.map(str::to_string),
        })
        .map_err(enc)?;
        self.call(ApiRequest::post("/v1/me/device").body(body)).await
    }

    // --- the CEC mesh ------------------------------------------------------

    /// `POST /v1/mesh/provision` — get the descriptor for the customer's
    /// isolated `cec-customer-<hash>` network.
    pub async fn provision_mesh(&self, device_id: &str) -> Result<MeshProvision, Error> {
        self.require_token()?;
        let body = serde_json::json!({ "device_id": device_id });
        self.call(ApiRequest::post("/v1/mesh/provision").body(body)).await
    }

    // --- Private Line ------------------------------------------------------

    /// `POST /v1/private-line` — rent a new Private Line venue.
    pub async fn rent_private_line(&self, label: Option<&str>) -> Result<PrivateLine, Error> {
        self.require_token()?;
        let body = serde_json::to_value(RentPrivateLine {
            label: label.map(str::to_string),
        })
        .map_err(enc)?;
        self.call(ApiRequest::post("/v1/private-line").body(body)).await
    }

    /// `GET /v1/private-line` — the customer's Private Lines.
    pub async fn list_private_lines(&self) -> Result<Vec<PrivateLine>, Error> {
        self.require_token()?;
        self.call(ApiRequest::get("/v1/private-line")).await
    }

    /// `DELETE /v1/private-line/{id}` — cancel a Private Line.
    pub async fn cancel_private_line(&self, id: &str) -> Result<(), Error> {
        self.require_token()?;
        self.call_unit(ApiRequest::delete(format!("/v1/private-line/{id}"))).await
    }

    // --- Ask-for-Help (customer) ------------------------------------------

    /// `POST /v1/help` — open a help session for an already-minted help room.
    pub async fn ask_for_help(&self, req: &AskForHelp) -> Result<HelpSession, Error> {
        self.require_token()?;
        let body = serde_json::to_value(req).map_err(enc)?;
        self.call(ApiRequest::post("/v1/help").body(body)).await
    }

    /// `GET /v1/help/{id}` — poll a help session's status.
    pub async fn help_status(&self, id: &str) -> Result<HelpSession, Error> {
        self.require_token()?;
        self.call(ApiRequest::get(format!("/v1/help/{id}"))).await
    }

    /// `POST /v1/help/{id}/cancel` — cancel a queued help session.
    pub async fn cancel_help(&self, id: &str) -> Result<(), Error> {
        self.require_token()?;
        self.call_unit(ApiRequest::post(format!("/v1/help/{id}/cancel"))).await
    }

    // --- agent side --------------------------------------------------------

    /// `POST /v1/agent/presence` — go online / offline.
    pub async fn set_presence(&self, online: bool) -> Result<AgentPresence, Error> {
        self.require_token()?;
        let body = serde_json::to_value(SetPresence { online }).map_err(enc)?;
        self.call(ApiRequest::post("/v1/agent/presence").body(body)).await
    }

    /// `GET /v1/agent/queue` — help sessions waiting for an online agent.
    pub async fn agent_queue(&self) -> Result<Vec<HelpSession>, Error> {
        self.require_token()?;
        self.call(ApiRequest::get("/v1/agent/queue")).await
    }

    /// `POST /v1/agent/help/{id}/accept` — take a session; receive the venue
    /// and room to join as the CEC Service node.
    pub async fn accept_help(&self, id: &str) -> Result<AgentAssignment, Error> {
        self.require_token()?;
        self.call(ApiRequest::post(format!("/v1/agent/help/{id}/accept"))).await
    }

    /// `POST /v1/agent/help/{id}/decline` — pass on a session.
    pub async fn decline_help(&self, id: &str) -> Result<(), Error> {
        self.require_token()?;
        self.call_unit(ApiRequest::post(format!("/v1/agent/help/{id}/decline"))).await
    }

    /// `POST /v1/agent/help/{id}/end` — end a session you're handling.
    pub async fn end_help(&self, id: &str) -> Result<(), Error> {
        self.require_token()?;
        self.call_unit(ApiRequest::post(format!("/v1/agent/help/{id}/end"))).await
    }

    // --- dev / mock-only ---------------------------------------------------

    /// `POST /v1/dev/grant` — MOCK ONLY. Set up entitlements / agent role.
    pub async fn dev_grant(&self, grant: &DevGrant) -> Result<(), Error> {
        let body = serde_json::to_value(grant).map_err(enc)?;
        self.call_unit(ApiRequest::post("/v1/dev/grant").body(body)).await
    }

    /// Issue an arbitrary request — an escape hatch for tooling. Prefer the
    /// typed methods above.
    pub async fn raw(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value, Error> {
        let mut req = ApiRequest::new(method, path);
        req.body = body;
        let resp = {
            req.bearer = self.token.clone();
            self.transport.send(req).await?
        };
        if resp.is_success() {
            Ok(resp.body)
        } else {
            Err(api_error(resp.status, resp.body))
        }
    }
}

fn enc(e: serde_json::Error) -> Error {
    Error::Decode(e.to_string())
}

/// Turn a non-2xx response into a structured [`Error::Api`].
fn api_error(status: u16, body: Value) -> Error {
    let parsed: Option<ApiErrorBody> = body
        .get("error")
        .and_then(|e| {
            if e.is_string() {
                e.as_str().map(|s| ApiErrorBody {
                    code: None,
                    message: Some(s.to_string()),
                })
            } else {
                serde_json::from_value(e.clone()).ok()
            }
        })
        .or_else(|| serde_json::from_value(body.clone()).ok());
    let (code, message) = match parsed {
        Some(b) => (b.code, b.message),
        None => (None, None),
    };
    Error::Api {
        status,
        code,
        message: message.unwrap_or_else(|| format!("HTTP {status}")),
    }
}
