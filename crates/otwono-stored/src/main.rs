//! `otwono-stored` — the OTWONO content store daemon.

#![forbid(unsafe_code)]

use otwono_proto::{Server, Shutdown};
use otwono_store::{StorageKey, Store, DEFAULT_KEY_PATH, DEFAULT_STORE_DIR};
use otwono_stored::StoreService;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

const USAGE: &str = "\
otwono-stored — OTWONO content store daemon

USAGE:
    otwono-stored [OPTIONS]

OPTIONS:
    --socket <PATH>        Control-plane socket (default $OTWONO_SOCKET_DIR/store.sock)
    --perm-socket <PATH>   Permission broker socket (default $OTWONO_SOCKET_DIR/perm.sock)
    --store-dir <PATH>     Where objects and chunks live (default /var/lib/otwono/store)
    --key <PATH>           Storage key, generated on first use (default /var/lib/otwono/storage.key)
    -h, --help             Show this message

EXIT CODES:
    0  clean shutdown
    1  usage error
    2  startup failure (unusable key, socket in use)
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
            eprintln!("otwono-stored: {m}\n\n{USAGE}");
            ExitCode::from(1)
        }
        Err(Error::Startup(m)) => {
            eprintln!("otwono-stored: {m}");
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
    let mut store_dir = PathBuf::from(DEFAULT_STORE_DIR);
    let mut key_path = PathBuf::from(DEFAULT_KEY_PATH);

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--socket" => socket = Some(next_path(&mut it, "--socket")?),
            "--perm-socket" => perm_socket = Some(next_path(&mut it, "--perm-socket")?),
            "--store-dir" => store_dir = next_path(&mut it, "--store-dir")?,
            "--key" => key_path = next_path(&mut it, "--key")?,
            "-h" | "--help" => return Ok(USAGE.to_string()),
            other => return Err(Error::Usage(format!("unknown option {other}"))),
        }
    }

    // Fails closed. A store the node cannot encrypt is a store that would sit on disk in
    // the clear, and starting anyway would mean the node believes its data is protected
    // when it is not.
    let (key, generated) =
        StorageKey::load_or_generate(&key_path).map_err(|e| Error::Startup(format!("storage key: {e}")))?;
    if generated {
        // Said once, loudly. From here the key is the only thing between a stolen disk and
        // its contents, and nothing backs it up.
        eprintln!(
            "otwono-stored: generated a storage key at {}\n  \
             Everything in {} is encrypted with it. It is not backed up and not \
             hardware-sealed: losing this file loses the store.",
            key_path.display(),
            store_dir.display()
        );
    }

    let store = Store::encrypted(&store_dir, key);
    store
        .ensure_layout()
        .map_err(|e| Error::Startup(format!("{}: {e}", store_dir.display())))?;

    let socket = socket.unwrap_or_else(|| otwono_proto::socket_path("store"));
    let perm_socket = perm_socket.unwrap_or_else(|| otwono_proto::socket_path("perm"));
    let server = Server::bind(&socket)
        .map_err(|e| Error::Startup(format!("cannot bind {}: {e}", socket.display())))?;
    eprintln!(
        "otwono-stored: listening on {} (store {}, encrypted, broker {})",
        socket.display(),
        store_dir.display(),
        perm_socket.display()
    );

    let service = Arc::new(StoreService::new(store, perm_socket));
    server
        .serve(service, Shutdown::new())
        .map_err(|e| Error::Startup(format!("serve failed: {e}")))?;
    Ok(String::new())
}

fn next_path<'a>(it: &mut impl Iterator<Item = &'a String>, flag: &str) -> Result<PathBuf, Error> {
    it.next()
        .map(PathBuf::from)
        .ok_or_else(|| Error::Usage(format!("{flag} needs a path")))
}
