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

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--socket" => socket = Some(next(&mut it, "--socket")?.into()),
            "--perm-socket" => perm_socket = Some(next(&mut it, "--perm-socket")?.into()),
            "--identity-dir" => identity_dir = next(&mut it, "--identity-dir")?.into(),
            "--listen" => listen = next(&mut it, "--listen")?,
            "--no-discovery" => discovery_enabled = false,
            "-h" | "--help" => return Ok(USAGE.to_string()),
            other => return Err(Error::Usage(format!("unknown option {other}"))),
        }
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
