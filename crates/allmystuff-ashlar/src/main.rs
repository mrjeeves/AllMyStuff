//! `allmystuff-ashlar` — the co-process Ashlar's `mesh.sites` space derives to.
//!
//! Not meant to be typed: the Ashlar runtime spawns it and speaks JSON Lines
//! on stdin/stdout. It is an ordinary binary all the same, so
//! `printf '{"call":"published","args":[]}' | allmystuff-ashlar` shows what a
//! site would see.
//!
//! Every answer is one line. A failure crosses as `{"error": …}` and Ashlar
//! raises it at the call site with the message intact — which is worth more
//! than a dead co-process, so this exits zero for a failed call and non-zero
//! only when stdin itself breaks.

use std::io::{BufRead, BufReader, Read, Write};

use allmystuff_ashlar::*;
use interprocess::local_socket::prelude::*;
#[cfg(unix)]
use interprocess::local_socket::GenericFilePath;
#[cfg(not(unix))]
use interprocess::local_socket::GenericNamespaced;
use interprocess::local_socket::Stream;
use serde_json::{json, Value};

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("allmystuff-ashlar: stdin closed badly: {e}");
                std::process::exit(1);
            }
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let answer = match serde_json::from_str::<Value>(line) {
            Ok(call) => match dispatch(&call) {
                Ok(value) => json!({ "ok": value }),
                Err(e) => json!({ "error": e }),
            },
            Err(e) => json!({ "error": format!("not a call envelope: {e}") }),
        };
        if writeln!(stdout, "{answer}").and_then(|_| stdout.flush()).is_err() {
            // Ashlar hung up. Nothing to report to, and nothing left to do.
            return;
        }
    }
}

fn dispatch(call: &Value) -> Result<Value, String> {
    let name = call
        .get("call")
        .and_then(Value::as_str)
        .ok_or("a call envelope needs a `call` name")?;
    let args = call
        .get("args")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    match name {
        "expose" => {
            let port = port_arg(&args, 0)?;
            let label = text_arg(&args, 1)?;
            let network = network_or_default(&text_arg(&args, 2).unwrap_or_default());
            expose(port, &label, &network)
        }
        "unexpose" => {
            let port = port_arg(&args, 0)?;
            let exposed = exposed_map(&node("site_exposed", json!({}))?);
            node(
                "site_set_exposed",
                json!({ "exposed": without_exposed(&exposed, port) }),
            )?;
            Ok(json!(true))
        }
        "published" => published(),
        "nearby" => nearby(),
        other => Err(format!(
            "no such call: `{other}`. This command answers Ashlar's `mesh.sites` \
             space: expose, unexpose, published, nearby. The roster itself is \
             `mesh`, which `myownmesh ashlar` answers."
        )),
    }
}

/// Publish a local port to the mesh's members: make sure this node is on that
/// mesh, then add the port to the node's exposed selection. The node is what
/// enforces the selection — its proxy dials nothing outside it — so this is
/// the whole of publishing, and unexposing is the whole of taking it back.
fn expose(port: u16, label: &str, network: &str) -> Result<Value, String> {
    let networks = node("mesh_networks", json!({}))?;
    if !already_joined(&networks, network) {
        // Open and auto-approving: everyone running the program should see
        // everyone else without a human approving each arrival, which makes
        // the mesh id itself the secret rather than the roster.
        node(
            "mesh_network_add",
            json!({ "config": {
                "id": network,
                "network_id": network,
                "label": label,
                "kind": "open",
                "auto_approve": true,
            }}),
        )?;
    }
    let exposed = exposed_map(&node("site_exposed", json!({}))?);
    node(
        "site_set_exposed",
        json!({ "exposed": with_exposed(&exposed, port, label) }),
    )?;
    let identity = node("mesh_identity", json!({})).unwrap_or(Value::Null);
    Ok(json!({
        "node": identity.get("device_id").and_then(Value::as_str).unwrap_or("this node"),
        "network": network,
        "label": label,
    }))
}

/// What this machine publishes, read back from the node's own selection rather
/// than from anything this process remembered — a worker that restarted still
/// answers with what is actually exposed.
fn published() -> Result<Value, String> {
    let exposed = exposed_map(&node("site_exposed", json!({}))?);
    let me = node("mesh_identity", json!({}))
        .ok()
        .and_then(|v| v.get("device_id").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| "this node".to_string());
    let mut out = Vec::new();
    for (id, label) in &exposed {
        let Some(port) = port_of(id) else { continue };
        let shown = if label.is_empty() { id.clone() } else { label.clone() };
        out.push(site(&me, &shown, &local_url(port)));
    }
    Ok(Value::Array(out))
}

/// The peers' sites, each with an address this machine can open. A peer's
/// advert says what it serves; mapping it here binds a local port that the
/// node proxies over the mesh, so the link an Ashlar page renders is an
/// ordinary loopback URL.
fn nearby() -> Result<Value, String> {
    let snapshot = node("session_snapshot", json!({}))?;
    let mappings = node("site_mappings", json!({})).unwrap_or(Value::Array(vec![]));
    let mut out = Vec::new();
    for (peer, port, label) in peer_sites(&snapshot) {
        let local = match existing_mapping(&mappings, &peer, port) {
            Some(p) => Some(p),
            None => node("site_map", json!({ "node": peer, "port": port }))
                .ok()
                .and_then(|v| v.get("localPort").and_then(Value::as_u64))
                .map(|p| p as u16),
        };
        // A site that would not map is still a site the peer is running, and
        // saying so with no link beats dropping it: "there but unreachable
        // from here" is a different fact from "not there".
        let url = local.map(local_url).unwrap_or_default();
        out.push(site(&peer, &label, &url));
    }
    Ok(Value::Array(out))
}

// -- the node's control socket ----------------------------------------------

/// One command to this machine's AllMyStuff node: connect, one frame out, one
/// frame in, close — the same short round trip its own GUI makes.
fn node(cmd: &str, args: Value) -> Result<Value, String> {
    let mut stream = connect()?;
    stream
        .write_all(&frame(cmd, args))
        .map_err(|e| format!("could not send `{cmd}` to the node: {e}"))?;
    stream
        .flush()
        .map_err(|e| format!("could not send `{cmd}` to the node: {e}"))?;

    let mut reader = BufReader::new(stream);
    let mut len_buf = [0u8; 4];
    reader
        .read_exact(&mut len_buf)
        .map_err(|e| format!("the node did not answer `{cmd}`: {e}"))?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len == 0 {
        return Err(format!("the node sent an empty frame for `{cmd}`"));
    }
    let mut body = vec![0u8; len];
    reader
        .read_exact(&mut body)
        .map_err(|e| format!("the node's answer to `{cmd}` was truncated: {e}"))?;
    if body[0] != TAG_JSON {
        return Err(format!(
            "the node answered `{cmd}` with a {} frame, not JSON",
            body[0]
        ));
    }
    unwrap_answer(&body[1..])
}

/// Where this machine's node listens. Derived exactly as the node derives it
/// (`node/src/node_control.rs`), including the `MYOWNMESH_HOME` override that
/// lets a second stack — CEC Support's, say — run beside an AllMyStuff install
/// without either finding the other's socket.
fn connect() -> Result<Stream, String> {
    let missing = "no AllMyStuff node is listening. Start one with `allmystuff serve`, \
                   or open the app — the mesh's roster is a separate space (`mesh`) and \
                   works without it.";
    #[cfg(unix)]
    {
        let home = std::env::var_os("MYOWNMESH_HOME")
            .map(std::path::PathBuf::from)
            .or_else(dirs::home_dir)
            .ok_or("could not resolve a home directory for the node socket")?
            .join(".myownmesh");
        let path = home.join("allmystuff-node.sock");
        let name = path
            .as_path()
            .to_fs_name::<GenericFilePath>()
            .map_err(|e| format!("{}: {e}", path.display()))?;
        Stream::connect(name).map_err(|e| format!("{missing} ({e})"))
    }
    #[cfg(not(unix))]
    {
        let name = "allmystuff-node"
            .to_ns_name::<GenericNamespaced>()
            .map_err(|e| format!("named pipe: {e}"))?;
        Stream::connect(name).map_err(|e| format!("{missing} ({e})"))
    }
}

fn text_arg(args: &[Value], index: usize) -> Result<String, String> {
    match args.get(index) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(other) => Err(format!("argument {} must be a text, not {other}", index + 1)),
        None => Err(format!("this call wants at least {} argument(s)", index + 1)),
    }
}

fn port_arg(args: &[Value], index: usize) -> Result<u16, String> {
    match args.get(index) {
        Some(Value::Number(n)) => n
            .as_u64()
            .filter(|p| *p > 0 && *p <= u16::MAX as u64)
            .map(|p| p as u16)
            .ok_or_else(|| format!("argument {} must be a port number", index + 1)),
        Some(other) => Err(format!("argument {} must be a port number, not {other}", index + 1)),
        None => Err(format!("this call wants at least {} argument(s)", index + 1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_call_names_both_halves_of_the_mesh() {
        // Reaching for the roster here is a real mistake to make, so the
        // message says which command answers it rather than just refusing.
        let e = dispatch(&json!({ "call": "peers", "args": [] })).unwrap_err();
        assert!(e.contains("no such call"), "{e}");
        assert!(e.contains("expose, unexpose, published, nearby"), "{e}");
        assert!(e.contains("myownmesh ashlar"), "{e}");
    }

    #[test]
    fn an_envelope_without_a_call_says_so() {
        let e = dispatch(&json!({ "args": [] })).unwrap_err();
        assert!(e.contains("needs a `call` name"), "{e}");
    }

    #[test]
    fn arguments_are_checked_before_the_node_is_touched() {
        // A bad argument must not reach the socket: the message an Ashlar
        // call site sees should be about the argument, not about a daemon.
        let e = dispatch(&json!({ "call": "expose", "args": ["8080"] })).unwrap_err();
        assert!(e.contains("must be a port number"), "{e}");
        let e = dispatch(&json!({ "call": "unexpose", "args": [] })).unwrap_err();
        assert!(e.contains("at least 1 argument"), "{e}");
        let e = dispatch(&json!({ "call": "expose", "args": [0] })).unwrap_err();
        assert!(e.contains("must be a port number"), "{e}");
        let e = dispatch(&json!({ "call": "expose", "args": [70000] })).unwrap_err();
        assert!(e.contains("must be a port number"), "{e}");
    }

    #[test]
    fn a_port_and_a_label_are_read_in_that_order() {
        assert_eq!(port_arg(&[json!(8080)], 0), Ok(8080));
        assert_eq!(text_arg(&[json!(8080), json!("app")], 1), Ok("app".to_string()));
        assert_eq!(
            text_arg(&[json!(8080)], 2),
            Err("this call wants at least 3 argument(s)".to_string())
        );
    }
}
