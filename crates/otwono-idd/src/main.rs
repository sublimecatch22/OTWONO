//! `otwono-idd` — the OTWONO identity daemon.

#![forbid(unsafe_code)]

use otwono_idd::IdentityService;
use otwono_identity::{migrate_combined, SharingKeystore, SigningKeystore, DEFAULT_IDENTITY_DIR};
use otwono_proto::{Server, Shutdown};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

const USAGE: &str = "\
otwono-idd — OTWONO identity daemon

USAGE:
    otwono-idd [OPTIONS]

OPTIONS:
    --socket <PATH>        Control-plane socket (default $OTWONO_SOCKET_DIR/id.sock)
    --perm-socket <PATH>   Permission broker socket (default $OTWONO_SOCKET_DIR/perm.sock)
    --identity-dir <PATH>  Keystore directory (default /var/lib/otwono/identity)
    --show                 Print this node's identity and exit
    -h, --help             Show this message

EXIT CODES:
    0  clean shutdown, or --show succeeded
    1  usage error
    2  startup failure (unreadable keystore, insecure key permissions, socket in use)
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
            eprintln!("otwono-idd: {m}\n\n{USAGE}");
            ExitCode::from(1)
        }
        Err(Error::Startup(m)) => {
            eprintln!("otwono-idd: {m}");
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
    let mut show = false;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--socket" => socket = Some(next_path(&mut it, "--socket")?),
            "--perm-socket" => perm_socket = Some(next_path(&mut it, "--perm-socket")?),
            "--identity-dir" => identity_dir = next_path(&mut it, "--identity-dir")?,
            "--show" => show = true,
            "-h" | "--help" => return Ok(USAGE.to_string()),
            other => return Err(Error::Usage(format!("unknown option {other}"))),
        }
    }

    // Split a pre-ADR-0010 keystore before anything reads it, so an upgraded node keeps
    // both its name and the agreement key its published node.pub already advertises.
    match migrate_combined(&identity_dir) {
        Ok(true) => eprintln!(
            "otwono-idd: split the combined keystore in {}; the agreement secret now lives in \
             {} and this daemon no longer holds it",
            identity_dir.display(),
            otwono_identity::AGREEMENT_KEY_FILE
        ),
        Ok(false) => {}
        Err(e) => return Err(Error::Startup(format!("keystore migration: {e}"))),
    }

    let keystore = SigningKeystore::new(&identity_dir);
    let (identity, generated) = keystore
        .load_or_generate()
        .map_err(|e| Error::Startup(format!("keystore: {e}")))?;

    if generated {
        // Say this once, loudly. Until the encrypted export exists, losing this file loses
        // the node's name and every peer relationship attached to it.
        eprintln!(
            "otwono-idd: generated a new node identity\n  \
             {}\n  fingerprint {}\n  \
             This key is not backed up and not hardware-sealed. Losing {} loses this identity.",
            identity.node_id(),
            identity.node_id().fingerprint(),
            keystore.key_path().display()
        );
    }

    // ADR-0019: the third key. Generated on first boot even on a node that never shares
    // anything, because being sealable-to is what makes a node addressable as a recipient
    // — somebody else has to be able to seal to it before it knows it wants them to.
    let sharing_store = SharingKeystore::new(&identity_dir);
    let (sharing, sharing_generated) = sharing_store
        .load_or_generate()
        .map_err(|e| Error::Startup(format!("sharing keystore: {e}")))?;
    if sharing_generated {
        eprintln!(
            "otwono-idd: generated a sharing key at {}\n  \
             Losing it makes everything shared *to* this node unreadable; things shared \
             *by* it are unaffected. Not backed up, not hardware-sealed.",
            sharing_store.key_path().display()
        );
    }

    if show {
        return Ok(format!(
            "node_id     {}\nfingerprint {}\ncreated     {}\nkeystore    {}\nsharing_key {}\n",
            identity.node_id(),
            identity.node_id().fingerprint(),
            identity.created_at_unix_ms(),
            keystore.key_path().display(),
            data_encoding::BASE64.encode(&sharing.public()),
        ));
    }

    let socket = socket.unwrap_or_else(|| otwono_proto::socket_path("id"));
    let perm_socket = perm_socket.unwrap_or_else(|| otwono_proto::socket_path("perm"));
    let server = Server::bind(&socket)
        .map_err(|e| Error::Startup(format!("cannot bind {}: {e}", socket.display())))?;
    eprintln!(
        "otwono-idd: listening on {} as {} (broker {})",
        socket.display(),
        identity.node_id().fingerprint(),
        perm_socket.display()
    );

    // Constructing the service is what vouches for the sharing key on disk.
    let service = Arc::new(
        IdentityService::new(keystore, identity, sharing, perm_socket)
            .map_err(|e| Error::Startup(format!("cannot vouch for the sharing key: {e}")))?,
    );
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
