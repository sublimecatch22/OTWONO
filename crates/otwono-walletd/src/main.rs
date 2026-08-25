//! `otwono-walletd` — the OTWONO wallet daemon.

#![forbid(unsafe_code)]

use otwono_proto::{Server, Shutdown};
use otwono_walletd::{WalletService, DEFAULT_VAULT_PATH};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

const USAGE: &str = "\
otwono-walletd — OTWONO wallet daemon

Holds the household's money key. It has no network, holds nothing unlocked between calls,
and takes a passphrase per call rather than keeping a session (ADR-0023).

Most of what it does needs a person: wallet.create, wallet.sign and wallet.export_seed all
require confirmation. ADR-0024 built that channel, but an approval from the uid that asked is
refused and the shipped image runs everything as one uid -- so on a booted node today the
wallet can be read and nothing else.

USAGE:
    otwono-walletd [OPTIONS]

OPTIONS:
    --socket <PATH>        Control-plane socket (default $OTWONO_SOCKET_DIR/wallet.sock)
    --perm-socket <PATH>   Permission broker socket (default $OTWONO_SOCKET_DIR/perm.sock)
    --vault <PATH>         Encrypted seed (default /var/lib/otwono/wallet/seed.vault)
    --status               Say whether a wallet exists here, and exit
    -h, --help             Show this message

EXIT CODES:
    0  clean shutdown, or --status succeeded
    1  usage error
    2  startup failure (socket in use, unreadable vault)
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
            eprintln!("otwono-walletd: {m}\n\n{USAGE}");
            ExitCode::from(1)
        }
        Err(Error::Startup(m)) => {
            eprintln!("otwono-walletd: {m}");
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
    let mut vault = PathBuf::from(DEFAULT_VAULT_PATH);
    let mut status = false;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--socket" => socket = Some(next_path(&mut it, "--socket")?),
            "--perm-socket" => perm_socket = Some(next_path(&mut it, "--perm-socket")?),
            "--vault" => vault = next_path(&mut it, "--vault")?,
            "--status" => status = true,
            "-h" | "--help" => return Ok(USAGE.to_string()),
            other => return Err(Error::Usage(format!("unknown option {other}"))),
        }
    }

    // Deliberately no first-boot generation, and this is the difference from otwono-idd.
    // A node identity is generated unasked because a node needs a name to be addressable
    // at all. A wallet is not like that: creating one commits somebody to writing down 24
    // words they have not been shown, and a wallet nobody knows exists is a wallet whose
    // phrase nobody wrote down. So this daemon starts with no vault and says so.
    if status {
        let exists = vault.exists();
        return Ok(format!(
            "vault   {}\nexists  {}\n{}",
            vault.display(),
            exists,
            if exists {
                "An address cannot be shown without the passphrase: this node stores no \
                 public key in the clear.\n"
            } else {
                "No wallet here. Creating one needs a person to confirm, and that channel \
                 does not exist yet (Phase 7).\n"
            }
        ));
    }

    let socket = socket.unwrap_or_else(|| otwono_proto::socket_path("wallet"));
    let perm_socket = perm_socket.unwrap_or_else(|| otwono_proto::socket_path("perm"));
    let server = Server::bind(&socket)
        .map_err(|e| Error::Startup(format!("cannot bind {}: {e}", socket.display())))?;
    eprintln!(
        "otwono-walletd: listening on {} (vault {}, broker {})",
        socket.display(),
        vault.display(),
        perm_socket.display()
    );
    if !vault.exists() {
        eprintln!("otwono-walletd: no wallet at {}", vault.display());
    }

    let service = Arc::new(WalletService::new(vault, perm_socket));
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
