//! `otwono-netd` — the OTWONO node mesh daemon.

#![forbid(unsafe_code)]

use otwono_identity::{Keystore, DEFAULT_IDENTITY_DIR};
use otwono_net::{Discovery, TcpLink};
use otwono_netd::{run_discovery, run_listener, NetService, NetState, DEFAULT_PORT};
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
    --identity-dir <PATH>  Keystore directory (default /var/lib/otwono/identity)
    --listen <ADDR>        Overlay listen address (default 0.0.0.0:8443)
    --no-discovery         Do not announce or browse on the LAN
    --status               Query a running daemon and print its overlay status, then exit
    -h, --help             Show this message

EXIT CODES:
    0  clean shutdown
    1  usage error
    2  startup failure
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
    let mut identity_dir = PathBuf::from(DEFAULT_IDENTITY_DIR);
    let mut listen = format!("0.0.0.0:{DEFAULT_PORT}");
    let mut discovery_enabled = true;
    let mut status_only = false;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--socket" => socket = Some(next(&mut it, "--socket")?.into()),
            "--perm-socket" => perm_socket = Some(next(&mut it, "--perm-socket")?.into()),
            "--identity-dir" => identity_dir = next(&mut it, "--identity-dir")?.into(),
            "--listen" => listen = next(&mut it, "--listen")?,
            "--no-discovery" => discovery_enabled = false,
            "--status" => status_only = true,
            "-h" | "--help" => return Ok(USAGE.to_string()),
            other => return Err(Error::Usage(format!("unknown option {other}"))),
        }
    }

    // --status talks to a running daemon; it must not touch the keystore, or a second
    // invocation would try to generate an identity of its own.
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

    let keystore = Keystore::new(&identity_dir);
    let (identity, generated) = keystore
        .load_or_generate()
        .map_err(|e| Error::Startup(format!("keystore: {e}")))?;
    if generated {
        eprintln!(
            "otwono-netd: generated a new node identity {}",
            identity.node_id()
        );
    }
    let node_id = *identity.node_id();

    let listener =
        TcpLink::listen(&listen).map_err(|e| Error::Startup(format!("cannot listen on {listen}: {e}")))?;
    let bound = listener
        .local_addr()
        .map_err(|e| Error::Startup(format!("cannot read the listen address: {e}")))?;

    let state = Arc::new(NetState::new(identity));
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
    let perm_socket = perm_socket.unwrap_or_else(|| otwono_proto::socket_path("perm"));
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

fn next<'a>(it: &mut impl Iterator<Item = &'a String>, flag: &str) -> Result<String, Error> {
    it.next()
        .cloned()
        .ok_or_else(|| Error::Usage(format!("{flag} needs a value")))
}
