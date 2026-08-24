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
    if peers_only {
        let socket = socket.unwrap_or_else(|| otwono_proto::socket_path("net"));
        let perm_socket = perm_socket.unwrap_or_else(|| otwono_proto::socket_path("perm"));
        return peer_report(&socket, &perm_socket);
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

/// Print one line per known peer: state, address, and the last failure if there was one.
fn peer_report(socket: &std::path::Path, perm_socket: &std::path::Path) -> Result<String, Error> {
    let mut broker = otwono_proto::Client::connect_waiting(perm_socket, std::time::Duration::from_secs(5))
        .map_err(|e| {
            Error::Startup(format!(
                "cannot reach otwono-permd at {}: {e}",
                perm_socket.display()
            ))
        })?;
    let token = broker
        .call(
            "perm.request",
            serde_json::json!({ "action": "net.read", "reason": "otwono-netd --peers" }),
        )
        .map_err(|e| Error::Startup(format!("perm.request transport failure: {e}")))?
        .map_err(|e| Error::Startup(format!("perm.request refused: {}", e.message)))?;
    let token = token
        .get("token")
        .and_then(|t| t.as_str())
        .ok_or_else(|| Error::Startup("perm.request returned no token".into()))?
        .to_string();

    let mut client = otwono_proto::Client::connect_waiting(socket, std::time::Duration::from_secs(5))
        .map_err(|e| Error::Startup(format!("cannot reach otwono-netd at {}: {e}", socket.display())))?;
    let value = client
        .call_with_capability("net.peers", serde_json::json!({}), &token)
        .map_err(|e| Error::Startup(format!("net.peers transport failure: {e}")))?
        .map_err(|e| Error::Startup(format!("net.peers refused: {}", e.message)))?;

    let peers = value
        .get("peers")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();
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
