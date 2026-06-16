//! `allmystuff-cec-mock` — a local, in-memory CEC backend you can actually
//! run, so the full app and agent flows work without the hosted service.
//!
//! ```text
//! allmystuff-cec-mock                 # listen on 127.0.0.1:8787
//! allmystuff-cec-mock --port 9000
//! allmystuff-cec-mock --demo          # every account gets Concierge + is an agent
//! ```
//!
//! It is the exact same [`MockBackend`] the tests use — this binary is just a
//! tiny HTTP/1.1 socket around `MockBackend::handle`, with permissive CORS so
//! the app's webview can fetch venue files, and it prints the sign-in codes it
//! "emails" so you can complete a login locally.

use std::sync::Arc;

use allmystuff_cec::mock::{MockBackend, MockConfig};
use allmystuff_cec::model::{ConciergeTier, Entitlements};
use allmystuff_cec::transport::{ApiRequest, ApiResponse, Method};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[tokio::main]
async fn main() {
    let mut port: u16 = 8787;
    let mut demo = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" | "-p" => {
                port = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| die("--port wants a number"));
            }
            "--demo" => demo = true,
            "-h" | "--help" => {
                print_help();
                return;
            }
            other => die(&format!("unknown argument: {other}")),
        }
    }

    let cfg = if demo {
        MockConfig {
            default_entitlements: Entitlements {
                concierge: Some(ConciergeTier::PayAsYouGo),
                private_line: false,
                hardware: false,
            },
            everyone_is_agent: true,
        }
    } else {
        MockConfig::default()
    };
    let backend = Arc::new(MockBackend::with_config(cfg));

    let addr = format!("127.0.0.1:{port}");
    let listener = TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| die(&format!("bind {addr}: {e}")));
    eprintln!("allmystuff-cec-mock listening on http://{addr}");
    if demo {
        eprintln!("  --demo: every account gets Concierge (Pay as you go) and is an agent");
    }
    eprintln!("  point the app at:  http://{addr}");
    eprintln!("  sign-in codes are printed here (this mock prints what it would email)\n");

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let backend = backend.clone();
                tokio::spawn(async move {
                    if let Err(e) = serve_one(stream, backend).await {
                        eprintln!("connection error: {e}");
                    }
                });
            }
            Err(e) => eprintln!("accept error: {e}"),
        }
    }
}

/// Handle a single connection: read one HTTP request, dispatch, reply, close.
/// A mock doesn't need keep-alive, so `Connection: close` keeps the parser
/// honest and simple.
async fn serve_one(mut stream: TcpStream, backend: Arc<MockBackend>) -> std::io::Result<()> {
    let Some(raw) = read_request(&mut stream).await? else {
        return Ok(()); // empty/broken connection
    };

    let (head, body_bytes) = split_head(&raw);
    let head = String::from_utf8_lossy(head);
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method_str = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("/");

    // CORS preflight — let the webview fetch venue files cross-origin.
    if method_str == "OPTIONS" {
        return write_response(&mut stream, 204, &Value::Null, true).await;
    }

    let method = match method_str {
        "GET" => Method::Get,
        "POST" => Method::Post,
        "DELETE" => Method::Delete,
        _ => return write_response(&mut stream, 405, &err_body("method_not_allowed"), true).await,
    };

    let mut bearer = None;
    for line in lines {
        if let Some(v) = header_value(line, "authorization") {
            if let Some(tok) = v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")) {
                bearer = Some(tok.trim().to_string());
            }
        }
    }

    let (path, query) = split_target(target);
    let body = if body_bytes.is_empty() {
        None
    } else {
        match serde_json::from_slice::<Value>(body_bytes) {
            Ok(v) => Some(v),
            Err(_) => return write_response(&mut stream, 400, &err_body("bad_json"), true).await,
        }
    };

    let req = ApiRequest {
        method,
        path: path.clone(),
        query,
        body: body.clone(),
        bearer,
    };
    let ApiResponse { status, body: out } = backend.handle(req);

    // Surface the sign-in code on the console so a human can complete login.
    if path == "/v1/auth/start" && status == 200 {
        let email = body
            .as_ref()
            .and_then(|b| b.get("email"))
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        if let Some(code) = out.get("dev_code").and_then(|v| v.as_str()) {
            eprintln!("  → sign-in code for {email}: {code}");
        }
    }

    write_response(&mut stream, status, &out, true).await
}

/// Read until the end of headers, then read the declared body. Caps the read
/// so a mock can't be wedged by a giant request.
async fn read_request(stream: &mut TcpStream) -> std::io::Result<Option<Vec<u8>>> {
    const MAX: usize = 1 << 20; // 1 MiB is plenty for this contract
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];

    // Read until we have the full header block.
    loop {
        if let Some(idx) = find_double_crlf(&buf) {
            // Headers complete. Do we have the whole body yet?
            let head = &buf[..idx];
            let need = content_length(head);
            let have = buf.len() - (idx + 4);
            while buf.len() - (idx + 4) < need && buf.len() < MAX {
                let n = stream.read(&mut chunk).await?;
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            let _ = have;
            return Ok(Some(buf));
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(if buf.is_empty() { None } else { Some(buf) });
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX {
            return Ok(Some(buf));
        }
    }
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn split_head(raw: &[u8]) -> (&[u8], &[u8]) {
    match find_double_crlf(raw) {
        Some(idx) => (&raw[..idx], &raw[idx + 4..]),
        None => (raw, &[]),
    }
}

fn content_length(head: &[u8]) -> usize {
    let head = String::from_utf8_lossy(head);
    for line in head.split("\r\n") {
        if let Some(v) = header_value(line, "content-length") {
            if let Ok(n) = v.trim().parse::<usize>() {
                return n;
            }
        }
    }
    0
}

/// Case-insensitive `Header: value` extraction.
fn header_value(line: &str, name_lower: &str) -> Option<String> {
    let (name, value) = line.split_once(':')?;
    if name.trim().eq_ignore_ascii_case(name_lower) {
        Some(value.trim().to_string())
    } else {
        None
    }
}

/// Split a request target into a path and decoded query pairs.
fn split_target(target: &str) -> (String, Vec<(String, String)>) {
    match target.split_once('?') {
        Some((path, qs)) => {
            let query = qs
                .split('&')
                .filter(|p| !p.is_empty())
                .map(|pair| match pair.split_once('=') {
                    Some((k, v)) => (url_decode(k), url_decode(v)),
                    None => (url_decode(pair), String::new()),
                })
                .collect();
            (path.to_string(), query)
        }
        None => (target.to_string(), Vec::new()),
    }
}

/// Minimal percent-decoding (and `+` → space) for query values.
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = hex_val(bytes[i + 1]);
                let lo = hex_val(bytes[i + 2]);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push(hi << 4 | lo);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

async fn write_response(
    stream: &mut TcpStream,
    status: u16,
    body: &Value,
    cors: bool,
) -> std::io::Result<()> {
    let payload = if body.is_null() {
        Vec::new()
    } else {
        serde_json::to_vec(body).unwrap_or_default()
    };
    let reason = reason_phrase(status);
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        payload.len()
    );
    if cors {
        head.push_str(
            "Access-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST, DELETE, OPTIONS\r\nAccess-Control-Allow-Headers: authorization, content-type\r\n",
        );
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    if !payload.is_empty() {
        stream.write_all(&payload).await?;
    }
    stream.flush().await?;
    let _ = stream.shutdown().await;
    Ok(())
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        422 => "Unprocessable Entity",
        _ => "Error",
    }
}

fn err_body(code: &str) -> Value {
    serde_json::json!({ "error": { "code": code, "message": code } })
}

fn print_help() {
    eprintln!(
        "allmystuff-cec-mock — a local reference CEC backend\n\n\
         USAGE:\n  allmystuff-cec-mock [--port <n>] [--demo]\n\n\
         OPTIONS:\n  \
         -p, --port <n>   port to listen on (default 8787)\n  \
         --demo           grant every account Concierge + agent role\n  \
         -h, --help       show this help\n"
    );
}

fn die(msg: &str) -> ! {
    eprintln!("allmystuff-cec-mock: {msg}");
    std::process::exit(2);
}
