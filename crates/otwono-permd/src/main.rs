//! `otwono-permd` — the OTWONO permission broker daemon.

#![forbid(unsafe_code)]

use otwono_permd::{AuditLog, Broker, Policy, DEFAULT_AUDIT_LOG, DEFAULT_POLICY_DIR};
use otwono_proto::{Server, Shutdown};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

const USAGE: &str = "\
otwono-permd — OTWONO permission broker

USAGE:
    otwono-permd [OPTIONS]

OPTIONS:
    --socket <PATH>       Control-plane socket (default $OTWONO_SOCKET_DIR/perm.sock)
    --confirm-socket <PATH>
                          Where a person answers confirmations (default
                          $OTWONO_SOCKET_DIR/confirm.sock). Deliberately a different socket
                          from --socket: see ADR-0024.
    --policy-dir <PATH>   Policy directory (default /etc/otwono/policy.d)
    --audit-log <PATH>    Audit log (default /var/log/otwono/audit.jsonl)
    --check               Load and validate the policy, then exit
    --verify-audit <PATH> Check an audit log's hash chain and exit
    -h, --help            Show this message

EXIT CODES:
    0  clean shutdown, or --check passed
    1  usage error
    2  startup failure (bad policy, unwritable audit log, socket in use)
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
            eprintln!("otwono-permd: {m}\n\n{USAGE}");
            ExitCode::from(1)
        }
        Err(Error::Startup(m)) => {
            eprintln!("otwono-permd: {m}");
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
    let mut confirm_socket: Option<PathBuf> = None;
    let mut policy_dir = PathBuf::from(DEFAULT_POLICY_DIR);
    let mut audit_log = PathBuf::from(DEFAULT_AUDIT_LOG);
    let mut check_only = false;
    let mut verify_audit: Option<PathBuf> = None;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--confirm-socket" => {
                confirm_socket = Some(
                    it.next()
                        .map(PathBuf::from)
                        .ok_or_else(|| Error::Usage("--confirm-socket needs a path".into()))?,
                )
            }
            "--socket" => {
                socket = Some(
                    it.next()
                        .map(PathBuf::from)
                        .ok_or_else(|| Error::Usage("--socket needs a path".into()))?,
                )
            }
            "--policy-dir" => {
                policy_dir = it
                    .next()
                    .map(PathBuf::from)
                    .ok_or_else(|| Error::Usage("--policy-dir needs a path".into()))?
            }
            "--audit-log" => {
                audit_log = it
                    .next()
                    .map(PathBuf::from)
                    .ok_or_else(|| Error::Usage("--audit-log needs a path".into()))?
            }
            "--check" => check_only = true,
            "--verify-audit" => {
                verify_audit = Some(
                    it.next()
                        .map(PathBuf::from)
                        .ok_or_else(|| Error::Usage("--verify-audit needs a path".into()))?,
                )
            }
            "-h" | "--help" => return Ok(USAGE.to_string()),
            other => return Err(Error::Usage(format!("unknown option {other}"))),
        }
    }

    // Verification reads a log and exits; it needs no policy and no socket, so a damaged
    // policy file must not stop an operator from checking their audit trail.
    if let Some(path) = verify_audit {
        let report = otwono_permd::AuditLog::verify(&path)
            .map_err(|e| Error::Startup(format!("cannot read {}: {e}", path.display())))?;
        let summary = format!(
            "{}: {} record(s), chain {}\n",
            path.display(),
            report.records,
            if report.intact { "intact" } else { "BROKEN" }
        );
        if !report.intact {
            return Err(Error::Startup(format!(
                "{summary}first bad record: seq {:?} ({})",
                report.first_bad_seq,
                report.detail.unwrap_or_default()
            )));
        }
        return Ok(summary);
    }

    let policy = Policy::load_dir(&policy_dir).map_err(|e| Error::Startup(format!("{e}")))?;
    let registry = otwono_permd::ActionRegistry::builtin();
    policy
        .validate(&registry)
        .map_err(|e| Error::Startup(format!("{e}")))?;

    if policy.rules().is_empty() {
        eprintln!(
            "otwono-permd: warning: no rules in {}; every request will be denied",
            policy_dir.display()
        );
    }

    if check_only {
        return Ok(format!(
            "policy ok: {} rule(s) from {}\n",
            policy.rules().len(),
            policy_dir.display()
        ));
    }

    let audit =
        AuditLog::open(&audit_log).map_err(|e| Error::Startup(format!("cannot open the audit log: {e}")))?;
    let socket = socket.unwrap_or_else(|| otwono_proto::socket_path("perm"));
    let server = Server::bind(&socket)
        .map_err(|e| Error::Startup(format!("cannot bind {}: {e}", socket.display())))?;

    eprintln!(
        "otwono-permd: listening on {} ({} policy rules, audit {})",
        socket.display(),
        policy.rules().len(),
        audit_log.display()
    );

    let broker = Arc::new(Broker::new(policy, audit));

    // The confirmation surface, on its own socket (ADR-0024 §3). It shares the broker's
    // state but serves only confirm.*, so whatever can reach it can answer requests and
    // cannot make them.
    //
    // Bound before the control-plane socket is served, so there is no window in which a
    // request can be made that nobody could yet answer.
    let confirm_socket = confirm_socket.unwrap_or_else(|| otwono_proto::socket_path("confirm"));
    let confirm_server = Server::bind(&confirm_socket)
        .map_err(|e| Error::Startup(format!("cannot bind {}: {e}", confirm_socket.display())))?;
    eprintln!("otwono-permd: confirmations on {}", confirm_socket.display());
    // Stated plainly at startup, because a channel that cannot do its job should say so
    // rather than look like it is working (ADR-0024 §4).
    eprintln!(
        "otwono-permd: a confirmation is refused when the approver's uid matches the asker's. \
         On a node where both run as the same user this stops nothing"
    );
    let confirmations = Arc::new(broker.confirmations());
    let shutdown = Shutdown::new();
    let s = shutdown.clone();
    std::thread::spawn(move || confirm_server.serve(confirmations, s));

    server
        .serve(broker, shutdown)
        .map_err(|e| Error::Startup(format!("serve failed: {e}")))?;
    Ok(String::new())
}
