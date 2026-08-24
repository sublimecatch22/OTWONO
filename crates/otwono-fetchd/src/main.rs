//! `otwono-fetchd` — the OTWONO fetch daemon.

#![forbid(unsafe_code)]

use otwono_fetch::source::DEFAULT_SOURCE_DIR;
use otwono_fetch::spool::DEFAULT_SPOOL_DIR;
use otwono_fetch::SourceSet;
use otwono_fetchd::transport::UreqTransport;
use otwono_fetchd::{FetchService, DEFAULT_CALL_BYTES, DEFAULT_CALL_TIMEOUT, DEFAULT_SLACK_BYTES};
use otwono_proto::{Server, Shutdown};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

const USAGE: &str = "\
otwono-fetchd — OTWONO fetch daemon

The only component that makes outbound client connections to hosts off the mesh. Callers
name a source from the allow-list and a path under its prefix; they never supply a URL.

USAGE:
    otwono-fetchd [OPTIONS]

OPTIONS:
    --socket <PATH>        Control-plane socket (default $OTWONO_SOCKET_DIR/fetch.sock)
    --perm-socket <PATH>   Permission broker socket (default $OTWONO_SOCKET_DIR/perm.sock)
    --source-dir <PATH>    Allow-list directory (default /etc/otwono/fetch.d)
    --spool-dir <PATH>     Where partial downloads accumulate (default /var/lib/otwono/fetch)
    --call-bytes <N>       Bytes transferred per fetch.get call
    --call-timeout <SECS>  Wall-clock budget for one request
    --slack-bytes <N>      Spool space held back from any fetch
    --check                Load and validate the allow-list, print it, and exit
    -h, --help             Show this message

EXIT CODES:
    0  clean shutdown, or --check passed
    1  usage error
    2  startup failure (bad allow-list, socket in use)
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
            eprintln!("otwono-fetchd: {m}\n\n{USAGE}");
            ExitCode::from(1)
        }
        Err(Error::Startup(m)) => {
            eprintln!("otwono-fetchd: {m}");
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
    let mut source_dir = PathBuf::from(DEFAULT_SOURCE_DIR);
    let mut spool_dir = PathBuf::from(DEFAULT_SPOOL_DIR);
    let mut call_bytes = DEFAULT_CALL_BYTES;
    let mut call_timeout = DEFAULT_CALL_TIMEOUT;
    let mut slack_bytes = DEFAULT_SLACK_BYTES;
    let mut check = false;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--socket" => socket = Some(next_path(&mut it, "--socket")?),
            "--perm-socket" => perm_socket = Some(next_path(&mut it, "--perm-socket")?),
            "--source-dir" => source_dir = next_path(&mut it, "--source-dir")?,
            "--spool-dir" => spool_dir = next_path(&mut it, "--spool-dir")?,
            "--call-bytes" => call_bytes = next_u64(&mut it, "--call-bytes")?,
            "--slack-bytes" => slack_bytes = next_u64(&mut it, "--slack-bytes")?,
            "--call-timeout" => call_timeout = Duration::from_secs(next_u64(&mut it, "--call-timeout")?),
            "--check" => check = true,
            "-h" | "--help" => return Ok(USAGE.to_string()),
            other => return Err(Error::Usage(format!("unknown option {other}"))),
        }
    }

    // A malformed allow-list stops the daemon rather than starting it on half a policy.
    // The alternative is a node that permits something nobody wrote down.
    let sources = SourceSet::load_dir(&source_dir)
        .map_err(|e| Error::Startup(format!("{}: {e}", source_dir.display())))?;

    if check {
        let mut out = format!(
            "source-dir  {}\nspool-dir   {}\nsources     {}\n",
            source_dir.display(),
            spool_dir.display(),
            sources.all().len()
        );
        for s in sources.all() {
            out.push_str(&format!(
                "  {:<20} https://{}:{}{}  max {} bytes\n",
                s.id,
                s.host,
                s.port_or_default(),
                s.path_prefix,
                s.max_bytes
            ));
        }
        return Ok(out);
    }

    if sources.is_empty() {
        // Not fatal: a node with no allow-list is a node that fetches nothing, which is a
        // supported state and the shipped default. Say so, because a silent one looks the
        // same as a broken one.
        eprintln!(
            "otwono-fetchd: no sources in {}; this node will fetch nothing until an \
             operator adds one",
            source_dir.display()
        );
    }

    std::fs::create_dir_all(&spool_dir)
        .map_err(|e| Error::Startup(format!("{}: {e}", spool_dir.display())))?;

    let socket = socket.unwrap_or_else(|| otwono_proto::socket_path("fetch"));
    let perm_socket = perm_socket.unwrap_or_else(|| otwono_proto::socket_path("perm"));
    let server = Server::bind(&socket)
        .map_err(|e| Error::Startup(format!("cannot bind {}: {e}", socket.display())))?;
    eprintln!(
        "otwono-fetchd: listening on {} with {} source(s) (broker {}, spool {})",
        socket.display(),
        sources.all().len(),
        perm_socket.display(),
        spool_dir.display()
    );

    let service = Arc::new(
        FetchService::new(
            sources,
            spool_dir,
            perm_socket,
            Box::new(UreqTransport::new(call_timeout)),
        )
        .with_budgets(call_bytes, call_timeout, slack_bytes),
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

fn next_u64<'a>(it: &mut impl Iterator<Item = &'a String>, flag: &str) -> Result<u64, Error> {
    let raw = it
        .next()
        .ok_or_else(|| Error::Usage(format!("{flag} needs a number")))?;
    raw.parse()
        .map_err(|e| Error::Usage(format!("{flag}: {raw:?} is not a number: {e}")))
}
