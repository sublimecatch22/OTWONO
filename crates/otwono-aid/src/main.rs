//! `otwono-aid` — the OTWONO AI daemon.
//!
//! Backends are discovered on disk at startup, so this daemon answers what *this machine*
//! can run rather than what the build was compiled with. With no backend installed it
//! still serves the catalog and admission control, and refuses `ai.infer` with a reason.

#![forbid(unsafe_code)]

use otwono_ai::{Catalog, PublisherTrust, DEFAULT_MODEL_DIR};
use otwono_aid::AiService;
use otwono_capability::CapabilityProfile;
use otwono_proto::{Client, Server, Shutdown};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

/// Where trusted model publishers are configured.
const DEFAULT_PUBLISHER_DIR: &str = "/etc/otwono/publishers.d";

const USAGE: &str = "\
otwono-aid — OTWONO AI daemon

USAGE:
    otwono-aid [OPTIONS]

OPTIONS:
    --socket <PATH>        Control-plane socket (default $OTWONO_SOCKET_DIR/ai.sock)
    --perm-socket <PATH>   Permission broker socket (default $OTWONO_SOCKET_DIR/perm.sock)
    --model-dir <PATH>     Model catalog directory (default /var/lib/otwono/models)
    --hw-socket <PATH>     Hardware daemon socket (default $OTWONO_SOCKET_DIR/hw.sock)
    --publishers <PATH>    Trusted model publishers (default /etc/otwono/publishers.d)
    --root <PATH>          Filesystem root to discover AI backends under (default /)
    --allow-unconfined-backends
                           Run inference backends without kernel confinement. Off by
                           default: on a kernel without Landlock a backend refuses to
                           start rather than parse untrusted model files unconfined
                           (ADR-0012). This is how an operator overrides that.
    --capabilities         Print what this node can do and exit
    -h, --help             Show this message

EXIT CODES:
    0  clean shutdown, or --capabilities succeeded
    1  usage error
    2  startup failure

Inference requires a backend installed under /usr/libexec/otwono/ai-backends and
/usr/lib/otwono/ai; without one, ai.infer refuses. See docs/ai/AI-RUNTIME.md.
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
            eprintln!("otwono-aid: {m}\n\n{USAGE}");
            ExitCode::from(1)
        }
        Err(Error::Startup(m)) => {
            eprintln!("otwono-aid: {m}");
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
    let mut model_dir = PathBuf::from(DEFAULT_MODEL_DIR);
    let mut hw_socket: Option<PathBuf> = None;
    let mut publisher_dir = PathBuf::from(DEFAULT_PUBLISHER_DIR);
    let mut root = PathBuf::from("/");
    let mut allow_unconfined_backends = false;
    let mut capabilities_only = false;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--socket" => socket = Some(next_path(&mut it, "--socket")?),
            "--perm-socket" => perm_socket = Some(next_path(&mut it, "--perm-socket")?),
            "--model-dir" => model_dir = next_path(&mut it, "--model-dir")?,
            "--hw-socket" => hw_socket = Some(next_path(&mut it, "--hw-socket")?),
            "--publishers" => publisher_dir = next_path(&mut it, "--publishers")?,
            "--root" => root = next_path(&mut it, "--root")?,
            "--allow-unconfined-backends" => allow_unconfined_backends = true,
            "--capabilities" => capabilities_only = true,
            "-h" | "--help" => return Ok(USAGE.to_string()),
            other => return Err(Error::Usage(format!("unknown option {other}"))),
        }
    }

    // The capability profile comes from otwono-hwd, not from probing here. CLAUDE.md §2.6
    // puts that decision in exactly one place and this daemon is a consumer of it.
    //
    // This is not pedantry. An earlier version probed locally, and because this unit runs
    // with PrivateNetwork=yes it saw no interfaces, classified the network axis from an
    // empty namespace, and would have reported a different tier than otwono-hwd for the
    // same machine. Two processes deriving the tier independently is how they disagree.
    // Asking the one publisher also lets this daemon keep PrivateNetwork, which it should.
    let perm_socket = perm_socket.unwrap_or_else(|| otwono_proto::socket_path("perm"));
    let hw_socket = hw_socket.unwrap_or_else(|| otwono_proto::socket_path("hw"));
    let profile = fetch_profile(&hw_socket, &perm_socket)?;
    let catalog = Catalog::new(&model_dir);
    // A malformed trust store is a startup failure, not a silent fallback to trusting
    // nobody: an operator who fat-fingers a publisher key should be told, not quietly left
    // with a node that refuses every signed model for a reason it never reports.
    let trust = PublisherTrust::load_dir(&publisher_dir)
        .map_err(|e| Error::Startup(format!("publisher trust store: {e}")))?;

    // Probed once, here, and handed to the service. Re-probing per request would report a
    // backend the instant a file appeared during an upgrade, half-installed.
    let installs = otwono_ai::discover(&root);

    if capabilities_only {
        let backends: Vec<_> = installs.iter().map(|i| i.backend).collect();
        let (entries, problems) = catalog
            .list()
            .map_err(|e| Error::Startup(format!("cannot read the model catalog: {e}")))?;
        return Ok(format!(
            "tier         {}\naccelerator  {}\nbackends     {}\nlocal infer  {}\nmodels       {} ({} unusable manifest(s))\npublishers   {}\ncatalog      {}\n",
            profile.tier.as_str(),
            profile.axes.accelerator.as_str(),
            if backends.is_empty() {
                "none".to_string()
            } else {
                backends
                    .iter()
                    .map(|b| b.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            },
            if backends.is_empty() { "unavailable" } else { "available" },
            entries.len(),
            problems.len(),
            trust.len(),
            model_dir.display(),
        ));
    }

    // Before binding, so that a socket on disk implies a usable catalog layout. The
    // boot-time check relies on exactly that ordering.
    catalog
        .ensure_layout()
        .map_err(|e| Error::Startup(format!("cannot create the model catalog: {e}")))?;

    let socket = socket.unwrap_or_else(|| otwono_proto::socket_path("ai"));
    let server = Server::bind(&socket)
        .map_err(|e| Error::Startup(format!("cannot bind {}: {e}", socket.display())))?;
    eprintln!(
        "otwono-aid: listening on {} (tier {}, backends: {})",
        socket.display(),
        profile.tier.as_str(),
        if installs.is_empty() {
            "none".to_string()
        } else {
            installs
                .iter()
                .map(|i| i.backend.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        }
    );

    server
        .serve(
            Arc::new(
                AiService::new(catalog, profile, trust, perm_socket, installs)
                    .with_unconfined_backends(allow_unconfined_backends),
            ),
            Shutdown::new(),
        )
        .map_err(|e| Error::Startup(format!("serve failed: {e}")))?;
    Ok(String::new())
}

/// Ask `otwono-hwd` for this machine's capability profile.
///
/// Fails closed. A daemon that cannot learn the tier cannot answer whether a model fits,
/// and guessing — or falling back to a local probe that may disagree — is how the two
/// answers drift apart without anyone noticing.
fn fetch_profile(hw_socket: &Path, perm_socket: &Path) -> Result<CapabilityProfile, Error> {
    let wait = Duration::from_secs(30);
    let mut broker = Client::connect_waiting(perm_socket, wait).map_err(|e| {
        Error::Startup(format!(
            "cannot reach the permission broker at {}: {e}",
            perm_socket.display()
        ))
    })?;
    let token = broker
        .call(
            "perm.request",
            serde_json::json!({
                "action": "hw.read",
                "reason": "otwono-aid needs the capability tier to decide what models fit",
            }),
        )
        .map_err(|e| Error::Startup(format!("perm.request transport failure: {e}")))?
        .map_err(|e| {
            Error::Startup(format!(
                "policy refuses otwono-aid the hw.read capability ({}); without the tier it \
                 cannot say what this node can run",
                e.message
            ))
        })?;
    let token = token
        .get("token")
        .and_then(|t| t.as_str())
        .ok_or_else(|| Error::Startup("perm.request returned no token".into()))?
        .to_string();

    let mut hwd = Client::connect_waiting(hw_socket, wait)
        .map_err(|e| Error::Startup(format!("cannot reach otwono-hwd at {}: {e}", hw_socket.display())))?;
    let value = hwd
        .call_with_capability("hw.profile", serde_json::json!({}), &token)
        .map_err(|e| Error::Startup(format!("hw.profile transport failure: {e}")))?
        .map_err(|e| Error::Startup(format!("hw.profile refused: {}", e.message)))?;
    serde_json::from_value(value).map_err(|e| {
        Error::Startup(format!(
            "otwono-hwd returned a profile this build cannot parse: {e}"
        ))
    })
}

fn next_path<'a>(it: &mut impl Iterator<Item = &'a String>, flag: &str) -> Result<PathBuf, Error> {
    it.next()
        .map(PathBuf::from)
        .ok_or_else(|| Error::Usage(format!("{flag} needs a path")))
}
