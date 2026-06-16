//! The HTTP transport seam.
//!
//! [`CecClient`](crate::CecClient) is generic over a [`Transport`] so the same
//! client code drives the real backend (`reqwest`), an in-process mock (for
//! tests, no sockets), or anything else. Futures are `Send` so the Tauri
//! backend can hold a `CecClient<ReqwestTransport>` inside an async command.

use std::future::Future;

use serde_json::Value;

use crate::Error;

/// The HTTP method an endpoint uses. (The CEC contract only needs these
/// three.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Delete,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Delete => "DELETE",
        }
    }
}

/// One request to the backend, already resolved to a path (the transport
/// prepends the base URL) plus an optional bearer token and JSON body.
#[derive(Debug, Clone)]
pub struct ApiRequest {
    pub method: Method,
    /// Leading-slash path, e.g. `/v1/me`.
    pub path: String,
    pub query: Vec<(String, String)>,
    pub body: Option<Value>,
    pub bearer: Option<String>,
}

impl ApiRequest {
    pub fn new(method: Method, path: impl Into<String>) -> Self {
        ApiRequest {
            method,
            path: path.into(),
            query: Vec::new(),
            body: None,
            bearer: None,
        }
    }
    pub fn get(path: impl Into<String>) -> Self {
        Self::new(Method::Get, path)
    }
    pub fn post(path: impl Into<String>) -> Self {
        Self::new(Method::Post, path)
    }
    pub fn delete(path: impl Into<String>) -> Self {
        Self::new(Method::Delete, path)
    }
    pub fn body(mut self, body: Value) -> Self {
        self.body = Some(body);
        self
    }
    pub fn bearer(mut self, token: Option<String>) -> Self {
        self.bearer = token;
        self
    }
    pub fn query(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.query.push((key.into(), value.into()));
        self
    }
}

/// The raw HTTP response — status plus parsed JSON body (or `Value::Null` for
/// an empty body). The client turns non-2xx into [`Error::Api`].
#[derive(Debug, Clone)]
pub struct ApiResponse {
    pub status: u16,
    pub body: Value,
}

impl ApiResponse {
    pub fn new(status: u16, body: Value) -> Self {
        ApiResponse { status, body }
    }
    pub fn ok(body: Value) -> Self {
        ApiResponse::new(200, body)
    }
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// The seam a [`CecClient`](crate::CecClient) talks through.
pub trait Transport {
    fn send(&self, req: ApiRequest) -> impl Future<Output = Result<ApiResponse, Error>> + Send;
}

/// The real HTTP transport: a `reqwest::Client` against a base URL.
#[cfg(feature = "reqwest")]
#[derive(Clone)]
pub struct ReqwestTransport {
    base_url: String,
    http: reqwest::Client,
}

#[cfg(feature = "reqwest")]
impl ReqwestTransport {
    /// Build a transport for `base_url` (e.g. `https://api.allmystuff.works`).
    /// Trailing slashes are trimmed so path joining stays predictable.
    pub fn new(base_url: impl Into<String>) -> Result<Self, Error> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("allmystuff-cec/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| Error::Transport(e.to_string()))?;
        Ok(ReqwestTransport {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            http,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

#[cfg(feature = "reqwest")]
impl Transport for ReqwestTransport {
    async fn send(&self, req: ApiRequest) -> Result<ApiResponse, Error> {
        let url = format!("{}{}", self.base_url, req.path);
        let method = match req.method {
            Method::Get => reqwest::Method::GET,
            Method::Post => reqwest::Method::POST,
            Method::Delete => reqwest::Method::DELETE,
        };
        let mut builder = self.http.request(method, &url);
        if !req.query.is_empty() {
            builder = builder.query(&req.query);
        }
        if let Some(token) = &req.bearer {
            builder = builder.bearer_auth(token);
        }
        if let Some(body) = &req.body {
            builder = builder.json(body);
        }
        let resp = builder
            .send()
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        let text = resp
            .text()
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        let body = if text.trim().is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).map_err(|e| Error::Decode(e.to_string()))?
        };
        Ok(ApiResponse { status, body })
    }
}
