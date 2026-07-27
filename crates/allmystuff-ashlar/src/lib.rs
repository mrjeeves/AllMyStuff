//! The Ashlar seam: answer an Ashlar program's `mesh.sites` space.
//!
//! Ashlar reaches everything outside its builtin set across one boundary. Two
//! space names derive to a co-process rather than to a library the project
//! supplies, because what they name belongs to the machine: `mesh` — who else
//! is on the private network this machine joined, answered by
//! `myownmesh ashlar` — and `mesh.sites`, answered here. Sites are the half
//! that needs a proxy able to carry a TCP connection to a peer, which is why
//! it is a separate space with a separate binding: a box can answer the roster
//! perfectly well and genuinely be unable to publish a site.
//!
//! Four calls, JSON Lines on stdin/stdout, shapes fixed by Ashlar's
//! `mesh.sites.Site` and `mesh.sites.Published`:
//!
//! | call | does |
//! |---|---|
//! | `expose(port, label, network)` | publish a local port to the mesh's members |
//! | `unexpose(port)` | take it back off |
//! | `published()` | what this machine is publishing |
//! | `nearby()` | the peers' sites, each with an address this machine can open |
//!
//! Publishing is **opt-in at the node**, and this does not change that: the
//! node advertises only the listening services its owner selected, and its
//! proxy refuses any port not on that list. `expose` adds one port to that
//! selection — the port an Ashlar program is serving, which its operator just
//! asked to publish — and `unexpose` removes it. Nothing here can reach a
//! service the owner never exposed, including through a peer.

use std::collections::BTreeMap;

use serde_json::{json, Value};

/// The mesh an Ashlar site lands on when nothing named one. It matches the
/// default in Ashlar's `lib/mesh` and in `myownmesh ashlar`, so a program that
/// says nothing and a machine told nothing meet on one area rather than two.
pub const DEFAULT_ASHLAR_NETWORK: &str = "ashlar";

/// What a node calls the listening service on `port`. The node's own scan
/// spells ids this way (`tcp:8080`), and `expose` needs the id before the scan
/// has necessarily noticed a port that came up a moment ago.
pub fn service_id(port: u16) -> String {
    format!("tcp:{port}")
}

/// The port an id names, if it names one. The inverse of [`service_id`], used
/// to answer `published` from the node's own selection rather than from
/// anything this process remembered.
pub fn port_of(id: &str) -> Option<u16> {
    id.strip_prefix("tcp:").and_then(|p| p.parse().ok())
}

/// The address this machine reaches a port at. Ashlar source may not write a
/// location down (its B5), so every URL a page renders arrives from out here,
/// at runtime.
pub fn local_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

/// Add one port to the node's exposed selection, leaving every other entry
/// alone. The node's map is `service id -> display label`; an empty label
/// means "use the scan's own name", so a program that named its site keeps
/// that name and one that did not is still exposed.
pub fn with_exposed(
    current: &BTreeMap<String, String>,
    port: u16,
    label: &str,
) -> BTreeMap<String, String> {
    let mut next = current.clone();
    next.insert(service_id(port), label.to_string());
    next
}

/// Remove one port from the selection. Removing what was never there is not an
/// error: `unexpose` runs on the way out of a run that may never have
/// published, and a shutdown path that can fail for that is worse than one
/// that cannot.
pub fn without_exposed(current: &BTreeMap<String, String>, port: u16) -> BTreeMap<String, String> {
    let mut next = current.clone();
    next.remove(&service_id(port));
    next
}

/// Read the node's exposed map out of a control answer.
pub fn exposed_map(value: &Value) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if let Some(obj) = value.as_object() {
        for (k, v) in obj {
            out.insert(k.clone(), v.as_str().unwrap_or("").to_string());
        }
    }
    out
}

/// One site on the mesh, in the shape Ashlar declared.
pub fn site(peer: &str, label: &str, url: &str) -> Value {
    json!({ "peer": peer, "label": label, "url": url })
}

/// Every site the peers in a node snapshot advertise, as `(node, port, label)`.
/// The snapshot carries each peer's published profile, and a profile's `sites`
/// are exactly what that peer chose to expose — so this reads an advert, never
/// a scan of somebody else's machine.
pub fn peer_sites(snapshot: &Value) -> Vec<(String, u16, String)> {
    let mut out = Vec::new();
    let Some(peers) = snapshot.get("peers").and_then(Value::as_array) else {
        return out;
    };
    for peer in peers {
        let node = peer
            .get("node")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let peer_label = peer.get("label").and_then(Value::as_str).unwrap_or("");
        if node.is_empty() {
            continue;
        }
        for advert in peer
            .get("sites")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            let Some(port) = advert.get("port").and_then(Value::as_u64) else {
                continue;
            };
            let label = advert
                .get("label")
                .and_then(Value::as_str)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("{peer_label} :{port}"));
            out.push((node.clone(), port as u16, label));
        }
    }
    out
}

/// The local port a node's site is already mapped to, if it is. Mapping is
/// what makes a peer's site openable from here, and asking first keeps
/// `nearby` from re-binding a port on every render.
pub fn existing_mapping(mappings: &Value, node: &str, port: u16) -> Option<u16> {
    mappings
        .as_array()?
        .iter()
        .find(|m| {
            m.get("node").and_then(Value::as_str) == Some(node)
                && m.get("port").and_then(Value::as_u64) == Some(port as u64)
        })
        .and_then(|m| m.get("localPort").and_then(Value::as_u64))
        .map(|p| p as u16)
}

/// Whether the node is already on this mesh, from a `mesh_networks` answer.
/// Joining is idempotent at the daemon, but asking first keeps a program's
/// every start from writing a config it already has.
pub fn already_joined(networks: &Value, network: &str) -> bool {
    let list = networks
        .get("networks")
        .and_then(Value::as_array)
        .cloned()
        .or_else(|| networks.as_array().cloned())
        .unwrap_or_default();
    list.iter().any(|n| {
        n.get("network_id").and_then(Value::as_str) == Some(network)
            || n.get("id").and_then(Value::as_str) == Some(network)
    })
}

/// The mesh a call named, or the default when it named none.
pub fn network_or_default(named: &str) -> String {
    let named = named.trim();
    if named.is_empty() {
        DEFAULT_ASHLAR_NETWORK.to_string()
    } else {
        named.to_string()
    }
}

// ---------------------------------------------------------------------------
// The node's control wire
// ---------------------------------------------------------------------------

/// A JSON-bodied frame. The node's wire is `[u32 BE len][1 tag byte][payload]`,
/// where `len` counts the tag — defined in `node/src/node_control.rs`, and
/// re-implemented here rather than linked so this seam stays out of the media
/// stack's build. Only the JSON tag is used: the byte-tagged frames carry media
/// batches, which no Ashlar call asks for.
pub const TAG_JSON: u8 = 0;

/// Frame one request for the node.
pub fn frame(cmd: &str, args: Value) -> Vec<u8> {
    let body = serde_json::to_vec(&json!({ "cmd": cmd, "args": args }))
        .expect("a JSON object always serializes");
    let len = (body.len() as u32) + 1;
    let mut out = Vec::with_capacity(body.len() + 5);
    out.extend_from_slice(&len.to_be_bytes());
    out.push(TAG_JSON);
    out.extend_from_slice(&body);
    out
}

/// Unwrap the node's `{ok, result, error}` answer, keeping the node's own
/// words on failure: the message an Ashlar call site raises should be the one
/// the node wrote, not a paraphrase of it.
pub fn unwrap_answer(payload: &[u8]) -> Result<Value, String> {
    let value: Value = serde_json::from_slice(payload)
        .map_err(|e| format!("the node's answer was not JSON: {e}"))?;
    if value.get("ok").and_then(Value::as_bool) == Some(true) {
        return Ok(value.get("result").cloned().unwrap_or(Value::Null));
    }
    Err(value
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("the node refused without saying why")
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_service_id_and_its_port_are_inverses() {
        assert_eq!(service_id(8080), "tcp:8080");
        assert_eq!(port_of("tcp:8080"), Some(8080));
        assert_eq!(port_of("udp:8080"), None);
        assert_eq!(port_of("tcp:not-a-port"), None);
    }

    #[test]
    fn exposing_leaves_every_other_selection_alone() {
        // The node's exposed map is its owner's choice about the WHOLE
        // machine. Publishing an Ashlar site adds one port to it; replacing
        // the map would silently unpublish whatever else was there.
        let mut current = BTreeMap::new();
        current.insert("tcp:3000".to_string(), "dev server".to_string());
        let after = with_exposed(&current, 8080, "enclave.app");
        assert_eq!(
            after.get("tcp:3000").map(String::as_str),
            Some("dev server")
        );
        assert_eq!(
            after.get("tcp:8080").map(String::as_str),
            Some("enclave.app")
        );

        let back = without_exposed(&after, 8080);
        assert_eq!(back, current, "withdrawing restores exactly what was there");
        assert_eq!(
            without_exposed(&current, 9999),
            current,
            "withdrawing what was never published is not an error"
        );
    }

    #[test]
    fn peer_sites_read_the_advert_not_a_scan() {
        let snapshot = json!({
            "peers": [
                { "node": "n1", "label": "ada", "sites": [
                    { "id": "tcp:8080", "label": "ada's pad", "port": 8080, "scheme": "http" },
                    { "id": "tcp:9000", "label": "", "port": 9000, "scheme": "http" }
                ]},
                { "node": "n2", "label": "grace", "sites": [] },
                { "label": "no id at all", "sites": [{ "port": 1 }] }
            ]
        });
        let sites = peer_sites(&snapshot);
        assert_eq!(
            sites.len(),
            2,
            "a peer with no node id is skipped: {sites:?}"
        );
        assert_eq!(sites[0], ("n1".to_string(), 8080, "ada's pad".to_string()));
        assert_eq!(
            sites[1],
            ("n1".to_string(), 9000, "ada :9000".to_string()),
            "an advert with no label falls back to the peer and port"
        );
    }

    #[test]
    fn an_empty_snapshot_yields_no_sites() {
        assert!(peer_sites(&json!({ "ready": false })).is_empty());
        assert!(peer_sites(&json!({ "peers": [] })).is_empty());
    }

    #[test]
    fn a_mapping_is_reused_rather_than_rebound() {
        let mappings = json!([
            { "node": "n1", "port": 8080, "localPort": 47001 },
            { "node": "n2", "port": 8080, "localPort": 47002 }
        ]);
        assert_eq!(existing_mapping(&mappings, "n1", 8080), Some(47001));
        assert_eq!(existing_mapping(&mappings, "n2", 8080), Some(47002));
        assert_eq!(existing_mapping(&mappings, "n3", 8080), None);
        assert_eq!(existing_mapping(&json!([]), "n1", 8080), None);
    }

    #[test]
    fn the_default_mesh_matches_what_ashlar_ships_with() {
        assert_eq!(DEFAULT_ASHLAR_NETWORK, "ashlar");
        assert_eq!(network_or_default(""), "ashlar");
        assert_eq!(network_or_default("  "), "ashlar");
        assert_eq!(network_or_default(" enclave "), "enclave");
    }

    #[test]
    fn a_joined_network_is_recognised_either_way_it_is_keyed() {
        let networks = json!({ "networks": [{ "id": "home", "network_id": "abc123" }] });
        assert!(already_joined(&networks, "abc123"));
        assert!(already_joined(&networks, "home"));
        assert!(!already_joined(&networks, "elsewhere"));
        assert!(!already_joined(&json!({}), "abc123"));
    }

    #[test]
    fn frame_roundtrips_the_node_wire() {
        // The length counts the tag byte, big-endian, and the body is the
        // node's `{cmd, args}` request. This is the whole contract with
        // `node/src/node_control.rs`; if it drifts, this fails rather than a
        // user's first request.
        let bytes = frame("site_exposed", json!({}));
        let len = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        assert_eq!(len, bytes.len() - 4, "len covers tag + payload");
        assert_eq!(bytes[4], TAG_JSON);
        let body: Value = serde_json::from_slice(&bytes[5..]).unwrap();
        assert_eq!(body["cmd"], "site_exposed");
        assert_eq!(body["args"], json!({}));
    }

    #[test]
    fn a_refusal_keeps_the_nodes_own_words() {
        let ok = unwrap_answer(br#"{"ok":true,"result":{"localPort":47001}}"#).unwrap();
        assert_eq!(ok["localPort"], 47001);
        let e = unwrap_answer(br#"{"ok":false,"error":"port is not advertised"}"#).unwrap_err();
        assert_eq!(e, "port is not advertised");
        let e = unwrap_answer(br#"{"ok":false}"#).unwrap_err();
        assert!(e.contains("without saying why"), "{e}");
        let e = unwrap_answer(b"not json").unwrap_err();
        assert!(e.contains("was not JSON"), "{e}");
    }

    #[test]
    fn exposed_map_reads_the_nodes_selection() {
        let value = json!({ "tcp:8080": "enclave.app", "tcp:3000": "" });
        let map = exposed_map(&value);
        assert_eq!(map.get("tcp:8080").map(String::as_str), Some("enclave.app"));
        assert_eq!(map.get("tcp:3000").map(String::as_str), Some(""));
        assert!(exposed_map(&json!(null)).is_empty());
    }

    #[test]
    fn a_url_is_built_out_here_never_in_ashlar_source() {
        assert_eq!(local_url(47001), "http://127.0.0.1:47001");
        assert_eq!(
            site("ada", "pad", &local_url(1)),
            json!({
                "peer": "ada", "label": "pad", "url": "http://127.0.0.1:1"
            })
        );
    }
}
