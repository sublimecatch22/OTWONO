//! `otwono-hwd` — the OTWONO hardware daemon.

#![forbid(unsafe_code)]

use otwono_capability::CapabilityOverrides;
use otwono_hwd::HwService;
use otwono_proto::{Server, Shutdown};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

const USAGE: &str = "\
otwono-hwd — OTWONO hardware daemon

USAGE:
    otwono-hwd [OPTIONS]

OPTIONS:
    --socket <PATH>        Control-plane socket (default $OTWONO_SOCKET_DIR/hw.sock)
    --perm-socket <PATH>   Permission broker socket (default $OTWONO_SOCKET_DIR/perm.sock)
    --root <PATH>          Probe root; use a fixture directory for testing (default /)
    --overrides <PATH>     Capability override file (default /etc/otwono/capability.override.toml)
    --no-overrides         Ignore the override file
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
            eprintln!("otwono-hwd: {m}\n\n{USAGE}");
            ExitCode::from(1)
        }
        Err(Error::Startup(m)) => {
            eprintln!("otwono-hwd: {m}");
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
    let mut root = PathBuf::from("/");
    let mut overrides_path: Option<PathBuf> = None;
    let mut use_overrides = true;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--socket" => socket = Some(next_path(&mut it, "--socket")?),
            "--perm-socket" => perm_socket = Some(next_path(&mut it, "--perm-socket")?),
            "--root" => root = next_path(&mut it, "--root")?,
            "--overrides" => overrides_path = Some(next_path(&mut it, "--overrides")?),
            "--no-overrides" => use_overrides = false,
            "-h" | "--help" => return Ok(USAGE.to_string()),
            other => return Err(Error::Usage(format!("unknown option {other}"))),
        }
    }

    let overrides = if use_overrides {
        let p = overrides_path
            .unwrap_or_else(|| PathBuf::from(otwono_capability::overrides::DEFAULT_OVERRIDE_PATH));
        CapabilityOverrides::load(&p).map_err(|e| Error::Startup(e.to_string()))?
    } else {
        CapabilityOverrides::default()
    };

    let socket = socket.unwrap_or_else(|| otwono_proto::socket_path("hw"));
    let perm_socket = perm_socket.unwrap_or_else(|| otwono_proto::socket_path("perm"));

    let server = Server::bind(&socket)
        .map_err(|e| Error::Startup(format!("cannot bind {}: {e}", socket.display())))?;
    eprintln!(
        "otwono-hwd: listening on {} (broker {}, probe root {})",
        socket.display(),
        perm_socket.display(),
        root.display()
    );

    let service = Arc::new(HwService::new(root, perm_socket, overrides));
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
