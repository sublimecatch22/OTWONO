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
    --no-replicate         Never hold a replica for the cluster, whatever policy allows
    --export-dir <PATH>    Where objects fetched with to_file are written
                           (default /var/lib/otwono/net-export)
    --identity-dir <PATH>  Keystore directory (default /var/lib/otwono/identity)
    --listen <ADDR>        Overlay listen address (default 0.0.0.0:8443)
    --no-discovery         Do not announce or browse on the LAN
    --status               Query a running daemon and print its overlay status, then exit
    --peers                Query a running daemon and print its peer table, then exit
    --peer-binding <PATH>  Write a connected peer's sharing binding to PATH, so
    --peer <NODE_ID>       Which peer --peer-binding should write; the first, if omitted
    --peer-ids             Print connected peers' full NodeIDs, one per line, for scripts
    --carry                Take at most one envelope into custody from each connected peer
    --collect              Collect what connected peers are holding for this node, then exit
    --mail                 Ask what is waiting for this node without fetching any of it.
                           The daemon collects on its own; this is for looking, and for
                           telling apart mail that arrived from mail that was fetched by hand
                           something can seal to it (ADR-0019). Exits non-zero if no
                           connected peer has published one.
    --shared-with-me       Ask every connected peer what it has sealed to this node
    --pointer <SVC/NAME>   Ask every connected peer what that name points at, print
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
    let mut replicate = true;
    let mut export_dir = PathBuf::from("/var/lib/otwono/net-export");
    let mut identity_dir = PathBuf::from(DEFAULT_IDENTITY_DIR);
    let mut listen = format!("0.0.0.0:{DEFAULT_PORT}");
    let mut discovery_enabled = true;
    let mut status_only = false;
    let mut peers_only = false;
    let mut shared_with_me = false;
    let mut pointer: Option<String> = None;
    let mut peer_binding: Option<PathBuf> = None;
    let mut peer_wanted: Option<String> = None;
    let mut peer_ids = false;
    let mut carry = false;
    let mut collect = false;
    let mut mail = false;
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
            "--no-replicate" => replicate = false,
            "--export-dir" => export_dir = next(&mut it, "--export-dir")?.into(),
            "--identity-dir" => identity_dir = next(&mut it, "--identity-dir")?.into(),
            "--listen" => listen = next(&mut it, "--listen")?,
            "--no-discovery" => discovery_enabled = false,
            "--status" => status_only = true,
            "--peers" => peers_only = true,
            "--shared-with-me" => shared_with_me = true,
            "--pointer" => pointer = Some(next(&mut it, "--pointer")?),
            "--peer-binding" => peer_binding = Some(next(&mut it, "--peer-binding")?.into()),
            "--peer" => peer_wanted = Some(next(&mut it, "--peer")?.to_string()),
            "--peer-ids" => peer_ids = true,
            "--carry" => carry = true,
            "--collect" => collect = true,
            "--mail" => mail = true,
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

    if let Some(spec) = pointer {
        let socket = socket.unwrap_or_else(|| otwono_proto::socket_path("net"));
        let perm_socket = perm_socket.unwrap_or_else(|| otwono_proto::socket_path("perm"));
        return pointer_report(&socket, &perm_socket, &spec);
    }

    if let Some(path) = peer_binding {
        let socket = socket.unwrap_or_else(|| otwono_proto::socket_path("net"));
        let perm_socket = perm_socket.unwrap_or_else(|| otwono_proto::socket_path("perm"));
        return write_peer_binding(&socket, &perm_socket, &path, peer_wanted.as_deref());
    }
    if peer_ids {
        let socket = socket.unwrap_or_else(|| otwono_proto::socket_path("net"));
        let perm_socket = perm_socket.unwrap_or_else(|| otwono_proto::socket_path("perm"));
        return connected_peer_ids(&socket, &perm_socket);
    }
    if carry || collect || mail {
        let method = if carry {
            "net.carry"
        } else if collect {
            "net.collect"
        } else {
            "net.mail"
        };
        let socket = socket.unwrap_or_else(|| otwono_proto::socket_path("net"));
        let perm_socket = perm_socket.unwrap_or_else(|| otwono_proto::socket_path("perm"));
        return carriage_report(&socket, &perm_socket, method);
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

    let store_socket = store_socket.unwrap_or_else(|| otwono_proto::socket_path("store"));
    // Attached whatever else this node does. Remembering pointer sequences is not a service
    // offered to peers -- it is this node's own defence against being rolled back by one
    // (ADR-0027 §1), so it does not belong behind `--no-serve-content` and there is no flag
    // to turn it off. `pointer.write` in the broker is the operator's control, and a node
    // that is not granted it refuses pointer reads rather than doing them blind.
    let mut state = NetState::new(Arc::new(signer))
        .with_handoff(handoff)
        .with_pointer_memory(Arc::new(otwono_netd::content::BrokeredPointers::new(
            &store_socket,
            &perm_socket,
        )))
        // Collecting mail addressed to this node is not behind `--no-serve-content` either,
        // and for the reason above: it is not a service offered to peers. It is this node
        // receiving what was sent to it, and a node that has chosen not to answer other
        // people's requests has not thereby chosen to stop getting its own messages.
        // `store.write` in the broker is the control, and a node without it collects nothing
        // and says so once.
        .with_inbox(Arc::new(otwono_netd::content::BrokeredInbox::new(
            &store_socket,
            &perm_socket,
        )));
    if serve_content {
        // Not fatal if otwono-stored is not up: the responder connects per request and
        // refuses when it cannot, which is the same answer a peer gets for anything else it
        // may not have. A mesh that authenticates but serves nothing is still a mesh.
        eprintln!(
            "otwono-netd: serving peer content from {}",
            store_socket.display()
        );
        state = state.with_responder(ContentResponder::new(&store_socket, &perm_socket));
        // Replication needs the same store socket, and giving it one is not the decision to
        // replicate: the broker's `cache.replicate` is, and a stock image does not grant it
        // (ADR-0026 §10). Configuring it here and gating it there keeps the operator's
        // consent in one place instead of two that can disagree.
        if replicate {
            state = state.with_holder(Arc::new(otwono_netd::content::BrokeredCache::new(
                &store_socket,
                &perm_socket,
            )));
        } else {
            eprintln!("otwono-netd: holding no replicas for the cluster (--no-replicate)");
        }
        // Carriage is its own agreement (ADR-0028 §8), so it is attached separately from
        // replication and gated separately: `envelope.carry` in the broker is the operator's
        // control, and a stock image grants it to nobody.
        state = state.with_carrier(Arc::new(otwono_netd::content::BrokeredCarrier::new(
            &store_socket,
            &perm_socket,
        )));
    } else {
        // A node that will not serve does not replicate either. Holding a replica it would
        // never hand to anyone is storage spent on nothing.
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
        .serve(
            Arc::new(NetService::new(state, perm_socket).with_store_socket(store_socket)),
            Shutdown::new(),
        )
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
/// Ask every connected peer what one of *its* names points at (ADR-0027).
///
/// Every peer is asked for its own record under that name, not for one owner's record from
/// whoever has it: a pointer is only ever fetched from the node that signed it, so "the same
/// name on three peers" is three different pointers, and the output names each owner.
///
/// A peer that does not publish the name is skipped rather than reported. It is
/// indistinguishable from one that will not say, and printing "absent" for both would put a
/// claim in front of an operator that this node cannot support.
fn pointer_report(
    socket: &std::path::Path,
    perm_socket: &std::path::Path,
    spec: &str,
) -> Result<String, Error> {
    let (service, name) = spec
        .split_once('/')
        .ok_or_else(|| Error::Usage(format!("--pointer wants <service>/<name>, got {spec:?}")))?;
    if service.is_empty() || name.is_empty() {
        return Err(Error::Usage("--pointer wants <service>/<name>".into()));
    }

    let peers = peer_table(socket, perm_socket, "otwono-netd --pointer")?;
    let connected: Vec<&serde_json::Value> = peers
        .iter()
        .filter(|p| p.get("state").and_then(|s| s.as_str()) == Some("connected"))
        .collect();
    if connected.is_empty() {
        return Ok("no connected peers\n".to_string());
    }

    let token = request_token(perm_socket, "net.content", "otwono-netd --pointer")?;
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
                "net.pointer",
                serde_json::json!({
                    "node_id": node_id, "address": address,
                    "service": service, "name": name,
                }),
                &token,
            )
            .map_err(|e| Error::Startup(format!("net.pointer transport failure: {e}")))?;
        // One unreachable peer must not lose the answers from the others -- but a refusal
        // is not unreachability, and printing nothing for it would be the worst possible
        // report. A rollback especially: it means something on the path served this node a
        // record older than one it has already seen (ADR-0027 §1), and an operator must not
        // learn that as silence.
        let reply = match reply {
            Ok(reply) => reply,
            Err(e) => {
                out.push_str(&format!(
                    "{} {}/{} refused: {}\n",
                    node_id, service, name, e.message
                ));
                continue;
            }
        };
        let Some(record) = reply.get("record").filter(|r| !r.is_null()) else {
            continue;
        };
        out.push_str(&format!(
            "{} {}/{} sequence {} {}\n",
            node_id,
            service,
            name,
            record.get("sequence").and_then(|s| s.as_u64()).unwrap_or(0),
            record
                .get("content_id")
                .and_then(|c| c.as_str())
                .unwrap_or("(tombstone)")
        ));
    }
    if out.is_empty() {
        out.push_str("no connected peer publishes that name\n");
    }
    Ok(out)
}

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
/// Connected peers' full NodeIDs, one per line.
///
/// `--peers` prints the **fingerprint**, which is right for it: that list is for a person to
/// read, and a 59-character id on every line would make it unreadable. But a fingerprint is
/// truncated and is documented throughout this system as never being what a decision is made
/// against, so a script that needs to *name* a peer — to ask for its sharing binding, say, and
/// then seal data to that key — must not be reading it from there.
///
/// Its own flag rather than an extra column, so the human-readable list stays readable and
/// the machine-readable one is unambiguous about what it is.
fn connected_peer_ids(socket: &std::path::Path, perm_socket: &std::path::Path) -> Result<String, Error> {
    let peers = peer_table(socket, perm_socket, "otwono-netd --peer-ids")?;
    let mut out = String::new();
    for peer in peers {
        if peer.get("state").and_then(|s| s.as_str()) != Some("connected") {
            continue;
        }
        if let Some(id) = peer.get("node_id").and_then(|n| n.as_str()) {
            out.push_str(id);
            out.push('\n');
        }
    }
    Ok(out)
}

/// Drive one carriage method against every connected peer (ADR-0028).
///
/// Every peer, not the first: a carrier holds what it happens to have met, so a recipient
/// asking only one peer would collect only what that peer happens to carry. One unreachable
/// peer must not lose the answers from the others, and a refusal is printed rather than
/// dropped — a carriage question that fails silently is indistinguishable from one that found
/// nothing, and those are very different things to an operator.
fn carriage_report(
    socket: &std::path::Path,
    perm_socket: &std::path::Path,
    method: &str,
) -> Result<String, Error> {
    let peers = peer_table(socket, perm_socket, method)?;
    let connected: Vec<&serde_json::Value> = peers
        .iter()
        .filter(|p| p.get("state").and_then(|s| s.as_str()) == Some("connected"))
        .collect();
    if connected.is_empty() {
        return Ok("no connected peers\n".to_string());
    }
    let token = request_token(perm_socket, "net.content", method)?;
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
                method,
                serde_json::json!({ "node_id": node_id, "address": address }),
                &token,
            )
            .map_err(|e| Error::Startup(format!("{method} transport failure: {e}")))?;
        match reply {
            Ok(v) => {
                if let Some(taken) = v.get("taken").and_then(|t| t.as_str()) {
                    out.push_str(&format!("{node_id} took {taken}\n"));
                } else if let Some(list) = v.get("collected").and_then(|c| c.as_array()) {
                    for id in list.iter().filter_map(|i| i.as_str()) {
                        out.push_str(&format!("{node_id} collected {id}\n"));
                    }
                } else if let Some(list) = v.get("waiting").and_then(|w| w.as_array()) {
                    if list.is_empty() {
                        out.push_str(&format!("{node_id} holds nothing for this node\n"));
                    }
                    for e in list {
                        let id = e.get("envelope_id").and_then(|i| i.as_str()).unwrap_or("?");
                        let size = e.get("size_bytes").and_then(|b| b.as_u64()).unwrap_or(0);
                        out.push_str(&format!("{node_id} waiting {id} {size}\n"));
                    }
                } else if v.get("carrying").and_then(|c| c.as_bool()) == Some(false) {
                    out.push_str(&format!("{node_id} this node carries no mail\n"));
                } else {
                    // With the reason, when the pass gave one. `nothing` on its own sent a
                    // previous debugging attempt to the wrong half of the system.
                    match v.get("why").and_then(|w| w.as_str()) {
                        Some(why) => out.push_str(&format!("{node_id} nothing: {why}\n")),
                        None => out.push_str(&format!("{node_id} nothing\n")),
                    }
                }
            }
            Err(e) => out.push_str(&format!("{node_id} refused: {}\n", e.message)),
        }
    }
    if out.is_empty() {
        out.push_str("nothing to report\n");
    }
    Ok(out)
}

fn write_peer_binding(
    socket: &std::path::Path,
    perm_socket: &std::path::Path,
    out: &std::path::Path,
    want: Option<&str>,
) -> Result<String, Error> {
    let peers = peer_table(socket, perm_socket, "otwono-netd --peer-binding")?;
    let chosen = peers
        .iter()
        .find(|p| {
            p.get("state").and_then(|s| s.as_str()) == Some("connected")
                && p.get("sharing_binding").is_some()
                // With more than one peer, "the first connected" is whichever the table
                // happened to order first, and a caller that wants a *particular* peer's
                // binding has no way to say so. On two nodes that never mattered; on three it
                // is the difference between sealing to the node you meant and sealing to the
                // other one.
                && want.is_none_or(|w| p.get("node_id").and_then(|n| n.as_str()) == Some(w))
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
