//! The interactive half of `amst`: once a terminal route is live, put the
//! local terminal in raw mode and pump it against the route — keystrokes up
//! (`term_send` Data), PTY output down (`term_poll`), and the local window
//! size across (`term_send` Resize) — until the far shell ends or the route
//! drops. This is exactly what the desktop app's xterm.js tab does, with the
//! user's own terminal standing in for the emulator.

use std::io::Write as _;
use std::sync::Arc;
use std::time::Duration;

use allmystuff_session::TermEvent;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::client::{NodeClient, NodeEvent};

/// How often we drain output as a safety net even if a `term-ready` poke was
/// lost — the same 50 ms the GUI's watcher uses.
const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// A terminal's final media `Exit` and route-control `Teardown` travel on
/// separate lanes. Either one can win the race (or the best-effort media
/// frame can be lost), so the CLI also watches the node's authoritative route
/// state. Without this fallback an already-ended `amst --run` stayed alive
/// forever and looked like a connected terminal in the device drawer even
/// though the host had no attachable PTY left.
const ROUTE_STATE_INTERVAL: Duration = Duration::from_millis(250);

/// Restores the terminal out of raw mode on the way out — on a clean return or
/// an early `?`. (Release builds abort on panic, so there are no unwinds to
/// catch; every real exit path here is a normal return.)
struct RawGuard;

impl RawGuard {
    fn enable() -> std::io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        Ok(RawGuard)
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// The local terminal's size as `(cols, rows)`, falling back to 80×24 when it
/// can't be read (a pipe, a CI log) so the far PTY still gets a sane size.
fn local_size() -> (u16, u16) {
    crossterm::terminal::size().unwrap_or((80, 24))
}

/// Drive `route_id` interactively until it ends. Returns the far shell's exit
/// code (0 when it ended without one, e.g. killed). `events` is the node's
/// already-subscribed event stream; we filter it for this route's pokes.
/// `initial_input` (if any) is sent once the screen is first painted — the
/// `cd <dir>` an "open a terminal here" launch injects.
pub async fn run(
    client: Arc<NodeClient>,
    route_id: String,
    mut events: mpsc::Receiver<NodeEvent>,
    initial_input: Option<Vec<u8>>,
) -> Result<i32, String> {
    // Claim this route's buffered output.
    let token = client
        .request("term_watch", json!({ "route_id": route_id }))
        .await?
        .as_u64()
        .ok_or("term_watch didn't return a token")?;

    // From here on the terminal is raw; the guard restores it on every return.
    let _raw = RawGuard::enable().map_err(|e| format!("couldn't enter raw mode: {e}"))?;

    // Keystrokes: a blocking reader thread forwards raw stdin bytes (every
    // escape sequence, paste, and control char intact) to the loop.
    let (key_tx, mut key_rx) = mpsc::channel::<Vec<u8>>(64);
    spawn_stdin_reader(key_tx);

    // Tell the host our size up front, then track changes.
    let mut last_size = local_size();
    send_resize(&client, &route_id, last_size).await;

    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    let mut route_ticker = tokio::time::interval(ROUTE_STATE_INTERVAL);
    route_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut stdout = std::io::stdout();
    let mut stdin_open = true;
    let mut exit_code = 0i32;

    // Paint whatever was buffered before we subscribed (the prompt/scrollback).
    drain(&client, &route_id, &mut stdout).await;

    // Inject the launch's `cd` (or whatever) as if typed, now that the prompt
    // is up. A failure here is non-fatal — the session is still usable.
    if let Some(bytes) = initial_input {
        if !bytes.is_empty() {
            let _ = send_keys(&client, &route_id, bytes).await;
        }
    }

    loop {
        tokio::select! {
            biased;

            // Output pokes / exit / resize from the node.
            ev = events.recv() => match ev {
                Some(NodeEvent::Emit { event, payload }) => {
                    if let Some(code) = handle_event(&client, &route_id, &event, &payload, &mut stdout).await {
                        exit_code = code;
                        break;
                    }
                }
                // The node restarted or the event stream dropped — end cleanly.
                Some(NodeEvent::Restart) | None => break,
            },

            // Keystrokes → the far PTY.
            bytes = key_rx.recv(), if stdin_open => match bytes {
                Some(b) if !b.is_empty() => {
                    // A send failure means the route is gone (host left, torn
                    // down) — stop rather than spin.
                    if send_keys(&client, &route_id, b).await.is_err() {
                        break;
                    }
                }
                Some(_) => {}
                // Local stdin closed (redirected input drained). Stop reading
                // keys but keep showing output until the far shell ends.
                None => stdin_open = false,
            },

            // Safety drain + resize check.
            _ = ticker.tick() => {
                drain(&client, &route_id, &mut stdout).await;
                let size = local_size();
                if size != last_size {
                    last_size = size;
                    send_resize(&client, &route_id, size).await;
                }
            }

            // `term-exit` is the rich path (it carries the process status),
            // but teardown is independently authoritative. A host may end the
            // PTY and tear down the route before its final media frame reaches
            // this viewer; observe that state so the local `amst` process and
            // its route cannot become a phantom terminal forever.
            _ = route_ticker.tick() => {
                if let Ok(snap) = client.request("session_snapshot", Value::Null).await {
                    if !route_still_live(&snap, &route_id) {
                        break;
                    }
                }
            }
        }
    }

    // Best-effort teardown: release the output claim and tear the route down so
    // the host stops streaming to us (and reaps the shell if we were the last
    // attacher). The raw-mode guard restores the terminal as it drops.
    let _ = client
        .request(
            "term_unwatch",
            json!({ "route_id": route_id, "token": token }),
        )
        .await;
    let _ = client
        .request("disconnect_route", json!({ "route_id": route_id }))
        .await;
    Ok(exit_code)
}

/// After [`crate::wait_active`] returns, a route is expected to stay active.
/// Negotiating states are accepted defensively across a node snapshot rebuild;
/// a terminal state or a pruned/absent route means this viewer is finished.
fn route_still_live(snapshot: &Value, route_id: &str) -> bool {
    matches!(
        crate::route_state(snapshot, route_id).as_deref(),
        Some("active" | "offered" | "incoming")
    )
}

/// Handle one node event for our route. Returns `Some(code)` when the far shell
/// ended (the loop should stop), `None` otherwise.
async fn handle_event(
    client: &Arc<NodeClient>,
    route_id: &str,
    event: &str,
    payload: &Value,
    stdout: &mut std::io::Stdout,
) -> Option<i32> {
    match event {
        // The queue went non-empty — drain it now instead of waiting for the
        // next tick (a lost poke just costs that latency, never bytes).
        "allmystuff://term-ready" => {
            if payload.as_str() == Some(route_id) {
                drain(client, route_id, stdout).await;
            }
            None
        }
        // The far shell ended. Drain any final bytes, then report the code.
        "allmystuff://term-exit" => {
            if payload.get("route").and_then(Value::as_str) == Some(route_id) {
                drain(client, route_id, stdout).await;
                return Some(
                    payload
                        .get("code")
                        .and_then(Value::as_i64)
                        .map(|c| c as i32)
                        .unwrap_or(0),
                );
            }
            None
        }
        _ => None,
    }
}

/// Drain `term_poll`'s `[u32 le len][bytes]…` batch straight to stdout. The
/// bytes are raw PTY output (VT and all); we pass them through untouched.
async fn drain(client: &Arc<NodeClient>, route_id: &str, stdout: &mut std::io::Stdout) {
    let Ok(batch) = client
        .request_bytes("term_poll", json!({ "route_id": route_id }))
        .await
    else {
        return;
    };
    let mut wrote = false;
    for chunk in split_batch(&batch) {
        if !chunk.is_empty() {
            let _ = stdout.write_all(chunk);
            wrote = true;
        }
    }
    if wrote {
        let _ = stdout.flush();
    }
}

/// Split a `term_poll` batch (`[u32 le len][bytes]…`, the node's
/// `ByteQueues::poll` framing) into its chunks. A trailing truncated header or
/// chunk is dropped rather than guessed at — the next poll re-delivers it.
fn split_batch(batch: &[u8]) -> Vec<&[u8]> {
    let mut chunks = Vec::new();
    let mut i = 0usize;
    while i + 4 <= batch.len() {
        let len = u32::from_le_bytes([batch[i], batch[i + 1], batch[i + 2], batch[i + 3]]) as usize;
        i += 4;
        if i + len > batch.len() {
            break;
        }
        chunks.push(&batch[i..i + len]);
        i += len;
    }
    chunks
}

/// Send keystroke bytes to the far PTY (one `term_send` Data). `Err` means the
/// route is gone.
async fn send_keys(client: &Arc<NodeClient>, route_id: &str, bytes: Vec<u8>) -> Result<(), String> {
    let event = TermEvent::Data { bytes };
    let payload = serde_json::to_value(&event).map_err(|e| e.to_string())?;
    client
        .request(
            "term_send",
            json!({ "route_id": route_id, "event": payload }),
        )
        .await
        .map(|_| ())
}

/// Tell the host our emulator size so the shell relays out to fit.
async fn send_resize(client: &Arc<NodeClient>, route_id: &str, (cols, rows): (u16, u16)) {
    let event = TermEvent::Resize { cols, rows };
    if let Ok(payload) = serde_json::to_value(&event) {
        let _ = client
            .request(
                "term_send",
                json!({ "route_id": route_id, "event": payload }),
            )
            .await;
    }
}

/// Forward raw stdin bytes to `tx` on a dedicated thread. A blocking read is
/// the only way to get bytes the moment they're typed; the thread ends at EOF
/// (and the process exit on session end takes it down regardless).
fn spawn_stdin_reader(tx: mpsc::Sender<Vec<u8>>) {
    std::thread::spawn(move || {
        use std::io::Read as _;
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 4096];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{route_still_live, split_batch};
    use serde_json::json;

    /// `[u32 le len][bytes]` per chunk — the exact framing `ByteQueues::poll`
    /// emits and the GUI's watcher decodes.
    #[test]
    fn split_batch_decodes_length_prefixed_chunks() {
        // Two chunks: "ab" then "cde".
        let batch = [2, 0, 0, 0, b'a', b'b', 3, 0, 0, 0, b'c', b'd', b'e'];
        assert_eq!(split_batch(&batch), vec![&b"ab"[..], &b"cde"[..]]);
        // Empty batch ⇒ nothing.
        assert!(split_batch(&[]).is_empty());
        // A truncated trailing chunk (len says 4, only 1 byte follows) is
        // dropped, keeping the whole first chunk.
        let batch = [1, 0, 0, 0, b'x', 4, 0, 0, 0, b'y'];
        assert_eq!(split_batch(&batch), vec![&b"x"[..]]);
    }

    #[test]
    fn route_state_fallback_ends_a_terminal_after_teardown_or_prune() {
        let route = |state: &str| {
            json!({
                "routes": [{
                    "route": { "id": "terminal-route" },
                    "state": { "state": state }
                }]
            })
        };
        assert!(route_still_live(&route("active"), "terminal-route"));
        assert!(!route_still_live(&route("torn_down"), "terminal-route"));
        assert!(!route_still_live(
            &json!({ "routes": [] }),
            "terminal-route"
        ));
    }
}
