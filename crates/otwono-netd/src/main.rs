//! `otwono-netd` — the OTWONO node mesh daemon.

#![forbid(unsafe_code)]

use otwono_identity::{AgreementKeystore, SessionSigner, DEFAULT_IDENTITY_DIR};
use otwono_net::{Discovery, TcpLink};
use otwono_netd::{
    run_discovery, run_listener, BrokeredSigner, ContentResponder, NetService, NetState, DEFAULT_PORT,
};
use otwono_proto::{Server, Shutdown};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

const USAGE: &str = "\
otwono-netd — OTWONO node mesh daemon

USAGE:
    otwono-netd [OPTIONS]

OPTIONS:
    --socket <PATH>        Control-plane socket (default $OTWONO_SOCKET_DIR/net.sock)
    --perm-socket <PATH>   Permission broker socket (default $OTWONO_SOCKET_DIR/perm.sock)
    --id-socket <PATH>     Identity daemon socket (default $OTWONO_SOCKET_DIR/id.sock)
    --store-socket <PATH>  Content store socket (default $OTWONO_SOCKET_DIR/store.sock)
    --no-serve-content     Do not answer peers' content requests at all
    --export-dir <PATH>    Where objects fetched with to_file are written
                           (default /var/lib/otwono/net-export)
    --identity-dir <PATH>  Keystore directory (default /var/lib/otwono/identity)
    --listen <ADDR>        Overlay listen address (default 0.0.0.0:8443)
    --no-discovery         Do not announce or browse on the LAN
    --status               Query a running daemon and print its overlay status, then exit
    --peers                Query a running daemon and print its peer table, then exit
    --peer-binding <PATH>  Write the first connected peer's sharing binding to PATH, so
                           something can seal to it (ADR-0019). Exits non-zero if no
                           connected peer has published one.
    --shared-with-me       Ask every connected peer what it has sealed to this node, print
                           one content id and plaintext size per line, then exit (ADR-0020)
    --fetch <CONTENT_ID>   Fetch one object from every connected peer, then exit
    --to-file              Write the fetched object to a file instead of returning its
                           bytes. Required above the control-plane's inline cap (ADR-0018).
    --cache                Keep what was fetched in the cluster cache. Never the
                           default: caching a peer's content is storing bytes the operator
                           did not choose one at a time.
    -h, --help             Show this message

EXIT CODES:
    0  clean shutdown
    1  usage error
    2  startup failure

Content served to peers comes from otwono-stored, and this daemon holds only the
store.serve capability: it cannot read a PRIVATE or SHARED object even if asked to. It
checks the label a second time itself before anything reaches a link.

This daemon holds only the node's X25519 agreement key. The Ed25519 key that its NodeID
names belongs to otwono-idd, which must be running and must grant this daemon the
id.sign_session capability, or no peer can be authenticated.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(msg) => {
            if !msg.is_empty() {
                print!("{msg}");
            }
            ExitCode::SUCCESS
        }
        Err(Error::Usage(m)) => {
            eprintln!("otwono-netd: {m}\n\n{USAGE}");
            ExitCode::from(1)
        }
        Err(Error::Startup(m)) => {
            eprintln!("otwono-netd: {m}");
            ExitCode::from(2)
        }
    }
}

enum Error {
    Usage(String),
    Startup(String),
}

fn run(args: &[String]) -> Result<String, Error> {
    let mut socket: Option<PathBuf> = None;
    let mut perm_socket: Option<PathBuf> = None;
    let mut id_socket: Option<PathBuf> = None;
    let mut store_socket: Option<PathBuf> = None;
    let mut serve_content = true;
    let mut export_dir = PathBuf::from("/var/lib/otwono/net-export");
    let mut identity_dir = PathBuf::from(DEFAULT_IDENTITY_DIR);
    let mut listen = format!("0.0.0.0:{DEFAULT_PORT}");
    let mut discovery_enabled = true;
    let mut status_only = false;
    let mut peers_only = false;
    let mut shared_with_me = false;
    let mut peer_binding: Option<PathBuf> = None;
    let mut fetch_id: Option<String> = None;
    let mut fetch_to_file = false;
    let mut fetch_cache = false;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--socket" => socket = Some(next(&mut it, "--socket")?.into()),
            "--perm-socket" => perm_socket = Some(next(&mut it, "--perm-socket")?.into()),
            "--id-socket" => id_socket = Some(next(&mut it, "--id-socket")?.into()),
            "--store-socket" => store_socket = Some(next(&mut it, "--store-socket")?.into()),
            "--no-serve-content" => serve_content = false,
            "--export-dir" => export_dir = next(&mut it, "--export-dir")?.into(),
            "--identity-dir" => identity_dir = next(&mut it, "--identity-dir")?.into(),
            "--listen" => listen = next(&mut it, "--listen")?,
            "--no-discovery" => discovery_enabled = false,
            "--status" => status_only = true,
            "--peers" => peers_only = true,
            "--shared-with-me" => shared_with_me = true,
            "--peer-binding" => peer_binding = Some(next(&mut it, "--peer-binding")?.into()),
            "--fetch" => fetch_id = Some(next(&mut it, "--fetch")?),
            "--to-file" => fetch_to_file = true,
            "--cache" => fetch_cache = true,
            "-h" | "--help" => return Ok(USAGE.to_string()),
            other => return Err(Error::Usage(format!("unknown option {other}"))),
        }
    }

    // --status talks to a running daemon; it must not touch the keystore, or a second
    // invocation would try to generate an agreement key of its own.
    if status_only {
        let socket = socket.unwrap_or_else(|| otwono_proto::socket_path("net"));
        let mut client = otwono_proto::Client::connect_waiting(&socket, std::time::Duration::from_secs(10))
            .map_err(|e| {
            Error::Startup(format!("cannot reach otwono-netd at {}: {e}", socket.display()))
        })?;
        let value = client
            .call("net.status", serde_json::json!({}))
            .map_err(|e| Error::Startup(format!("net.status transport failure: {e}")))?
            .map_err(|e| Error::Startup(format!("net.status refused: {e}")))?;
        let get = |k: &str| value.get(k).cloned().unwrap_or(serde_json::Value::Null);
        return Ok(format!(
            "node_id     {}\nfingerprint {}\nlisten      {}\nknown       {}\nconnected   {}\n",
            get("node_id").as_str().unwrap_or("?"),
            get("fingerprint").as_str().unwrap_or("?"),
            get("listen_addr").as_str().unwrap_or("?"),
            get("peers_known").as_u64().unwrap_or(0),
            get("peers_connected").as_u64().unwrap_or(0),
        ));
    }

    // Who this node has met, and why any attempt failed. Guarded by net.read, so unlike
    // --status this asks otwono-permd for a token first: peer identities are
    // privacy-relevant even though each NodeID is public.
    //
    // This exists because a mesh that will not form is otherwise invisible on a headless
    // box — the reason lives in the journal, and "connected=0" does not say why.
    // Fetch one object from whichever peer this node has already authenticated.
    //
    // The peer is chosen here rather than named by the caller, because the caller that
    // needs this most is a shell script on a booted node that has no way to know a
    // neighbour's NodeID until the mesh has formed. It asks net.peers, takes the first
    // connected one, and fetches.
    if let Some(content_id) = fetch_id {
        let socket = socket.unwrap_or_else(|| otwono_proto::socket_path("net"));
        let perm_socket = perm_socket.unwrap_or_else(|| otwono_proto::socket_path("perm"));
        return fetch_from_a_peer(&socket, &perm_socket, &content_id, fetch_to_file, fetch_cache);
    }

    if peers_only {
        let socket = socket.unwrap_or_else(|| otwono_proto::socket_path("net"));
        let perm_socket = perm_socket.unwrap_or_else(|| otwono_proto::socket_path("perm"));
        return peer_report(&socket, &perm_socket);
    }

    if shared_with_me {
        let socket = socket.unwrap_or_else(|| otwono_proto::socket_path("net"));
        let perm_socket = perm_socket.unwrap_or_else(|| otwono_proto::socket_path("perm"));
        return shared_index_report(&socket, &perm_socket);
    }

    if let Some(path) = peer_binding {
        let socket = socket.unwrap_or_else(|| otwono_proto::socket_path("net"));
        let perm_socket = perm_socket.unwrap_or_else(|| otwono_proto::socket_path("perm"));
        return write_peer_binding(&socket, &perm_socket, &path);
    }

    // Only the agreement half. This daemon has no way to open node.key and no code path
    // that would know what to do with it (ADR-0010).
    let keystore = AgreementKeystore::new(&identity_dir);
    let (agreement, generated) = keystore
        .load_or_generate()
        .map_err(|e| Error::Startup(format!("agreement keystore: {e}")))?;
    if generated {
        eprintln!(
            "otwono-netd: generated a new agreement key in {}",
            keystore.key_path().display()
        );
    }

    // Register it with otwono-idd and take back the signed binding. The node's *name*
    // arrives here, from the signing key; this process cannot derive it.
    let perm_socket = perm_socket.unwrap_or_else(|| otwono_proto::socket_path("perm"));
    let id_socket = id_socket.unwrap_or_else(|| otwono_proto::socket_path("id"));
    let signer = BrokeredSigner::bind(agreement, &id_socket, &perm_socket)
        .map_err(|e| Error::Startup(format!("cannot bind this node's agreement key: {e}")))?;
    let node_id = signer.node_id();
    eprintln!(
        "otwono-netd: bound to {} via {} (the node key stays in otwono-idd)",
        node_id.fingerprint(),
        id_socket.display()
    );

    let listener =
        TcpLink::listen(&listen).map_err(|e| Error::Startup(format!("cannot listen on {listen}: {e}")))?;
    let bound = listener
        .local_addr()
        .map_err(|e| Error::Startup(format!("cannot read the listen address: {e}")))?;

    // Its own directory, not otwono-stored's: two daemons sharing one export directory
    // means each reaper can delete the other's in-flight file.
    let handoff = otwono_store::Handoff::new(&export_dir);
    handoff
        .ensure_layout()
        .map_err(|e| Error::Startup(format!("export directory: {e}")))?;
    if let Err(e) = handoff.reap(otwono_store::EXPORT_MAX_AGE) {
        eprintln!("otwono-netd: could not sweep {}: {e}", export_dir.display());
    }
    otwono_store::handoff::spawn_reaper(
        otwono_store::Handoff::new(&export_dir),
        otwono_store::handoff::REAP_INTERVAL,
        otwono_store::EXPORT_MAX_AGE,
    );

    let mut state = NetState::new(Arc::new(signer)).with_handoff(handoff);
    if serve_content {
        let store_socket = store_socket.unwrap_or_else(|| otwono_proto::socket_path("store"));
        // Not fatal if otwono-stored is not up: the responder connects per request and
        // refuses when it cannot, which is the same answer a peer gets for anything else it
        // may not have. A mesh that authenticates but serves nothing is still a mesh.
        eprintln!(
            "otwono-netd: serving peer content from {}",
            store_socket.display()
        );
        state = state.with_responder(ContentResponder::new(&store_socket, &perm_socket));
    } else {
        eprintln!("otwono-netd: not serving content to peers (--no-serve-content)");
    }
    let state = Arc::new(state);
    {
        let state = Arc::clone(&state);
        std::thread::spawn(move || run_listener(state, listener));
    }

    if discovery_enabled {
        match Discovery::start(&node_id, bound.port()) {
            Ok(discovery) => {
                eprintln!(
                    "otwono-netd: announcing {} on {}",
                    discovery.instance_name(),
                    otwono_net::SERVICE_TYPE
                );
                let state = Arc::clone(&state);
                std::thread::spawn(move || run_discovery(state, discovery));
            }
            // A node with no multicast is still a node: it can be dialled, and it can be
            // told about peers. Refusing to start would be worse than running without
            // discovery, so this is a warning rather than a failure.
            Err(e) => eprintln!("otwono-netd: LAN discovery unavailable, continuing without it: {e}"),
        }
    }

    let socket = socket.unwrap_or_else(|| otwono_proto::socket_path("net"));
    let server = Server::bind(&socket)
        .map_err(|e| Error::Startup(format!("cannot bind {}: {e}", socket.display())))?;
    eprintln!(
        "otwono-netd: listening on {} as {} (overlay {})",
        socket.display(),
        node_id.fingerprint(),
        bound
    );

    server
        .serve(Arc::new(NetService::new(state, perm_socket)), Shutdown::new())
        .map_err(|e| Error::Startup(format!("serve failed: {e}")))?;
    Ok(String::new())
}

/// Ask a capability of the broker, the way every other client must.
fn broker_token(perm_socket: &std::path::Path, action: &str, reason: &str) -> Result<String, Error> {
    let mut broker = otwono_proto::Client::connect_waiting(perm_socket, std::time::Duration::from_secs(5))
        .map_err(|e| {
            Error::Startup(format!(
                "cannot reach otwono-permd at {}: {e}",
                perm_socket.display()
            ))
        })?;
    broker
        .call(
            "perm.request",
            serde_json::json!({ "action": action, "reason": reason }),
        )
        .map_err(|e| Error::Startup(format!("perm.request transport failure: {e}")))?
        .map_err(|e| Error::Startup(format!("perm.request refused: {}", e.message)))?
        .get("token")
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .ok_or_else(|| Error::Startup("perm.request returned no token".into()))
}

/// Fetch one object from the first connected peer.
fn fetch_from_a_peer(
    socket: &std::path::Path,
    perm_socket: &std::path::Path,
    content_id: &str,
    to_file: bool,
    cache: bool,
) -> Result<String, Error> {
    let read_token = broker_token(perm_socket, "net.read", "otwono-netd --fetch: find a peer")?;
    let mut client = otwono_proto::Client::connect_waiting(socket, std::time::Duration::from_secs(5))
        .map_err(|e| Error::Startup(format!("cannot reach otwono-netd at {}: {e}", socket.display())))?;
    let peers = client
        .call_with_capability("net.peers", serde_json::json!({}), &read_token)
        .map_err(|e| Error::Startup(format!("net.peers transport failure: {e}")))?
        .map_err(|e| Error::Startup(format!("net.peers refused: {}", e.message)))?;

    // Every connected peer, not the first one. ADR-0015's whole claim is that any holder of
    // a chunk is as good as any other, so a fetch that used one peer when three were
    // reachable would leave the point of the design on the floor -- and would make the
    // fan-out path unreachable from a booted node.
    let candidates: Vec<serde_json::Value> = peers
        .get("peers")
        .and_then(|p| p.as_array())
        .into_iter()
        .flatten()
        .filter(|p| p.get("state").and_then(|s| s.as_str()) == Some("connected"))
        .filter_map(|p| {
            let node_id = p.get("node_id")?.as_str()?;
            let address = p.get("addresses")?.as_array()?.first()?.as_str()?;
            Some(serde_json::json!({ "node_id": node_id, "address": address }))
        })
        .collect();
    if candidates.is_empty() {
        return Err(Error::Startup("no connected peer to fetch from".into()));
    }
    let asked = candidates.len();

    let content_token = broker_token(
        perm_socket,
        "net.content",
        "otwono-netd --fetch: fetch an object from this node's peers",
    )?;
    let value = client
        .call_with_capability(
            "net.fetch",
            serde_json::json!({
                "peers": candidates,
                "content_id": content_id,
                "to_file": to_file,
                "cache": cache,
            }),
            &content_token,
        )
        .map_err(|e| Error::Startup(format!("net.fetch transport failure: {e}")))?
        .map_err(|e| Error::Startup(format!("net.fetch refused: {}", e.message)))?;

    // `served` is what shows whether the work actually spread. A shell check on a booted
    // node has no other way to see it, and "it completed" does not distinguish one peer
    // doing everything from three sharing it.
    let mut out = format!(
        "{} {} bytes visibility={} asked={asked} served={}",
        value.get("content_id").and_then(|v| v.as_str()).unwrap_or("?"),
        value.get("size_bytes").and_then(|v| v.as_u64()).unwrap_or(0),
        value.get("visibility").and_then(|v| v.as_str()).unwrap_or("?"),
        value
            .get("peers_that_served")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
    );
    if let Some(path) = value.get("path").and_then(|v| v.as_str()) {
        out.push_str(&format!(" path={path}"));
    }
    // Said either way. A caller that asked for caching and did not get it needs to know,
    // and one that did not ask needs to be able to see that nothing was kept.
    match value.get("cached") {
        Some(serde_json::Value::Bool(b)) => out.push_str(&format!(" cached={b}")),
        Some(other) => out.push_str(&format!(" cached={other}")),
        None => {}
    }
    out.push('\n');
    Ok(out)
}

/// Print one line per known peer: state, address, and the last failure if there was one.
/// Ask every connected peer what it has sealed to this node (ADR-0020).
///
/// Every peer, not the first: what a recipient wants is everything that is theirs, and which
/// neighbour happens to hold a given object is not something it can know in advance. Output
/// is one `<content_id> <plaintext_bytes> <peer>` per line, which is what a shell script can
/// read and what a person can skim.
///
/// A peer that answers nothing contributes nothing and is not an error: "nothing for you"
/// and "nothing for anybody" are the same answer by design, so there is no failure here to
/// report.
fn shared_index_report(socket: &std::path::Path, perm_socket: &std::path::Path) -> Result<String, Error> {
    let peers = peer_table(socket, perm_socket, "otwono-netd --shared-with-me")?;
    let connected: Vec<&serde_json::Value> = peers
        .iter()
        .filter(|p| p.get("state").and_then(|s| s.as_str()) == Some("connected"))
        .collect();
    if connected.is_empty() {
        return Ok("no connected peers\n".to_string());
    }

    let token = request_token(perm_socket, "net.content", "otwono-netd --shared-with-me")?;
    let mut client = otwono_proto::Client::connect_waiting(socket, std::time::Duration::from_secs(5))
        .map_err(|e| Error::Startup(format!("cannot reach otwono-netd at {}: {e}", socket.display())))?;

    let mut out = String::new();
    for peer in connected {
        let Some(node_id) = peer.get("node_id").and_then(|n| n.as_str()) else {
            continue;
        };
        let Some(address) = peer
            .get("addresses")
            .and_then(|a| a.as_array())
            .and_then(|a| a.first())
            .and_then(|a| a.as_str())
        else {
            continue;
        };
        let reply = client
            .call_with_capability(
                "net.shared_with_me",
                serde_json::json!({ "node_id": node_id, "address": address }),
                &token,
            )
            .map_err(|e| Error::Startup(format!("net.shared_with_me transport failure: {e}")))?;
        // One unreachable peer must not lose the answers from the others.
        let Ok(reply) = reply else { continue };
        for entry in reply
            .get("entries")
            .and_then(|e| e.as_array())
            .cloned()
            .unwrap_or_default()
        {
            out.push_str(&format!(
                "{} {} {}\n",
                entry.get("content_id").and_then(|v| v.as_str()).unwrap_or("?"),
                entry
                    .get("plaintext_size_bytes")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                node_id
            ));
        }
    }
    if out.is_empty() {
        out.push_str("nothing has been shared with this node\n");
    }
    Ok(out)
}

/// Write the first connected peer's sharing binding out, so something can seal to it.
///
/// The peer is chosen here rather than named by the caller, for the reason `--fetch` does
/// the same: the caller that needs this is a shell script on a booted node, which has no way
/// to know a neighbour's NodeID until the mesh has formed.
///
/// What is written is the *signed* binding, verbatim. Whatever seals to it verifies it
/// again for itself — this daemon checked it when the peer offered it, and a second check
/// costs nothing next to sealing somebody's data to a key nobody vouched for.
fn write_peer_binding(
    socket: &std::path::Path,
    perm_socket: &std::path::Path,
    out: &std::path::Path,
) -> Result<String, Error> {
    let peers = peer_table(socket, perm_socket, "otwono-netd --peer-binding")?;
    let chosen = peers
        .iter()
        .find(|p| {
            p.get("state").and_then(|s| s.as_str()) == Some("connected") && p.get("sharing_binding").is_some()
        })
        .ok_or_else(|| {
            Error::Startup(
                "no connected peer has published a sharing binding, so there is nobody to \
                 seal to"
                    .into(),
            )
        })?;
    let binding = chosen.get("sharing_binding").expect("filtered above");
    let text = serde_json::to_string_pretty(binding)
        .map_err(|e| Error::Startup(format!("cannot render the binding: {e}")))?;
    std::fs::write(out, text + "\n").map_err(|e| Error::Startup(format!("{}: {e}", out.display())))?;
    Ok(format!(
        "{} -> {}\n",
        chosen.get("fingerprint").and_then(|f| f.as_str()).unwrap_or("?"),
        out.display()
    ))
}

/// Ask the broker for one capability, on this caller's own authority.
fn request_token(perm_socket: &std::path::Path, action: &str, reason: &str) -> Result<String, Error> {
    let mut broker = otwono_proto::Client::connect_waiting(perm_socket, std::time::Duration::from_secs(5))
        .map_err(|e| {
            Error::Startup(format!(
                "cannot reach otwono-permd at {}: {e}",
                perm_socket.display()
            ))
        })?;
    broker
        .call(
            "perm.request",
            serde_json::json!({ "action": action, "reason": reason }),
        )
        .map_err(|e| Error::Startup(format!("perm.request transport failure: {e}")))?
        .map_err(|e| Error::Startup(format!("perm.request refused: {}", e.message)))?
        .get("token")
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .ok_or_else(|| Error::Startup("perm.request returned no token".into()))
}

/// Ask a running daemon for its peer table.
fn peer_table(
    socket: &std::path::Path,
    perm_socket: &std::path::Path,
    reason: &str,
) -> Result<Vec<serde_json::Value>, Error> {
    let token = request_token(perm_socket, "net.read", reason)?;
    let mut client = otwono_proto::Client::connect_waiting(socket, std::time::Duration::from_secs(5))
        .map_err(|e| Error::Startup(format!("cannot reach otwono-netd at {}: {e}", socket.display())))?;
    let value = client
        .call_with_capability("net.peers", serde_json::json!({}), &token)
        .map_err(|e| Error::Startup(format!("net.peers transport failure: {e}")))?
        .map_err(|e| Error::Startup(format!("net.peers refused: {}", e.message)))?;
    Ok(value
        .get("peers")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default())
}

fn peer_report(socket: &std::path::Path, perm_socket: &std::path::Path) -> Result<String, Error> {
    let peers = peer_table(socket, perm_socket, "otwono-netd --peers")?;
    if peers.is_empty() {
        return Ok("no peers known\n".to_string());
    }
    let mut out = String::new();
    for peer in peers {
        let get = |k: &str| peer.get(k).and_then(|v| v.as_str()).unwrap_or("?").to_string();
        let addresses = peer
            .get("addresses")
            .and_then(|a| a.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(","))
            .unwrap_or_default();
        let addresses = if addresses.is_empty() {
            "-".to_string()
        } else {
            addresses
        };
        out.push_str(&format!("{} {} {}", get("fingerprint"), get("state"), addresses));
        // Whether this peer can be sealed to, not the key itself: a fingerprint list is
        // something a person reads, and a base64 public key in it is noise.
        if peer.get("sharing_binding").is_some() {
            out.push_str(" sealable");
        }
        if let Some(err) = peer.get("last_error").and_then(|e| e.as_str()) {
            out.push_str(&format!(" error={err}"));
        }
        out.push('\n');
    }
    Ok(out)
}

fn next<'a>(it: &mut impl Iterator<Item = &'a String>, flag: &str) -> Result<String, Error> {
    it.next()
        .cloned()
        .ok_or_else(|| Error::Usage(format!("{flag} needs a value")))
}
