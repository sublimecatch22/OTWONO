//! `otwono-permctl` — answer confirmations, and read the audit chain.
//!
//! The channel ADR-0024 built had no client. A confirmation that nobody can answer is a
//! confirmation that never completes, so this is the piece that makes the mechanism usable
//! by the person it exists for.
//!
//! Two sockets, deliberately: `list`, `approve` and `deny` talk to the **confirmation**
//! socket, and `audit` talks to the control plane. That is not an implementation detail to
//! be tidied away — the separation is what the self-answering rule stands on (ADR-0024 §3),
//! and a CLI that hid it would invite somebody to merge them.

#![forbid(unsafe_code)]

use otwono_proto::Client;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
otwono-permctl — answer confirmations, and read the audit chain

An action flagged `always_confirm` does not proceed on policy alone. It waits here until
somebody answers, and only a subject the node designates as a confirmer may do so.

USAGE:
    otwono-permctl <COMMAND> [OPTIONS]

COMMANDS:
    list                      What is waiting for an answer
    approve <ID>              Approve one confirmation
    deny <ID>                 Deny one confirmation
    request <ACTION>          Ask for a capability. Prints a token, or the confirmation id
                              it is waiting on
    claim <ID>                Collect the token an approved confirmation authorised
    audit                     Verify the audit chain

OPTIONS:
    --resource <PATH>         What the request is about. Part of what a person is shown, and
                              part of what the approval covers
    --confirm-socket <PATH>   Confirmation socket (default $OTWONO_SOCKET_DIR/confirm.sock)
    --socket <PATH>           Control-plane socket (default $OTWONO_SOCKET_DIR/perm.sock)
    --json                    Print the daemon's reply verbatim
    -h, --help                Show this message

BEFORE YOU APPROVE:
    Read the resource, not just the action. \"Delete a file\" and \"delete which file\" are
    different questions, and the answer to the second is on the line.

    `claims` is whatever asked for the permission describing itself. It is an assertion, not
    a fact, and on a node running an agent it may be text the agent did not write.

EXIT CODES:
    0  done
    1  usage error
    2  the daemon refused, or could not be reached
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(out) => {
            print!("{out}");
            ExitCode::SUCCESS
        }
        Err(Error::Usage(m)) => {
            eprintln!("otwono-permctl: {m}\n\n{USAGE}");
            ExitCode::from(1)
        }
        Err(Error::Runtime(m)) => {
            eprintln!("otwono-permctl: {m}");
            ExitCode::from(2)
        }
    }
}

#[derive(Debug)]
enum Error {
    Usage(String),
    Runtime(String),
}

fn run(args: &[String]) -> Result<String, Error> {
    let mut command: Option<String> = None;
    let mut target: Option<String> = None;
    let mut confirm_socket: Option<PathBuf> = None;
    let mut socket: Option<PathBuf> = None;
    let mut resource: Option<String> = None;
    let mut as_json = false;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(USAGE.to_string()),
            "--json" => as_json = true,
            "--resource" => {
                resource = Some(
                    it.next()
                        .cloned()
                        .ok_or_else(|| Error::Usage("--resource needs a value".into()))?,
                )
            }
            "--confirm-socket" => confirm_socket = Some(next_path(&mut it, "--confirm-socket")?),
            "--socket" => socket = Some(next_path(&mut it, "--socket")?),
            other if other.starts_with('-') => return Err(Error::Usage(format!("unknown option {other}"))),
            other if command.is_none() => command = Some(other.to_string()),
            other if target.is_none() => target = Some(other.to_string()),
            other => return Err(Error::Usage(format!("unexpected argument {other}"))),
        }
    }

    let command = command.ok_or_else(|| Error::Usage("no command given".into()))?;
    let confirm = confirm_socket.unwrap_or_else(|| otwono_proto::socket_path("confirm"));
    let perm = socket.unwrap_or_else(|| otwono_proto::socket_path("perm"));

    let need_id = |what: &str| -> Result<String, Error> {
        target
            .clone()
            .ok_or_else(|| Error::Usage(format!("{what} needs a confirmation id")))
    };

    let (sock, method, params) = match command.as_str() {
        "list" => (&confirm, "confirm.list", json!({})),
        "approve" => (
            &confirm,
            "confirm.approve",
            json!({ "confirmation_id": need_id("approve")? }),
        ),
        "deny" => (
            &confirm,
            "confirm.deny",
            json!({ "confirmation_id": need_id("deny")? }),
        ),
        "request" => {
            let action = target
                .clone()
                .ok_or_else(|| Error::Usage("request needs an action".into()))?;
            let mut p = json!({ "action": action });
            if let Some(r) = &resource {
                p["resource"] = json!(r);
            }
            (&perm, "perm.request", p)
        }
        "claim" => (
            &perm,
            "perm.claim",
            json!({ "confirmation_id": need_id("claim")? }),
        ),
        "audit" => (&perm, "perm.audit.verify", json!({})),
        other => return Err(Error::Usage(format!("unknown command {other:?}"))),
    };

    let value = Client::connect(sock)
        .map_err(|e| Error::Runtime(format!("cannot reach {}: {e}", sock.display())))?
        .call(method, params)
        .map_err(|e| Error::Runtime(format!("{method} transport failure: {e}")))?
        .map_err(|e| Error::Runtime(format!("{method} refused: {}", e.message)))?;

    if as_json {
        return Ok(format!("{value}\n"));
    }
    Ok(render(&command, &value))
}

fn render(command: &str, v: &Value) -> String {
    match command {
        "list" => {
            let items = v["pending"].as_array().cloned().unwrap_or_default();
            if items.is_empty() {
                return "nothing is waiting for an answer\n".to_string();
            }
            let mut out = String::new();
            for p in &items {
                let s = |k: &str| p[k].as_str().unwrap_or("").to_string();
                out.push_str(&format!(
                    "{}\n  {} — {}\n  blast radius {}\n",
                    s("confirmation_id"),
                    s("action"),
                    s("summary"),
                    s("blast_radius"),
                ));
                // The resource is on its own line and never abbreviated: it is the half of
                // the question that says what will actually happen.
                match p["resource"].as_str() {
                    Some(r) => out.push_str(&format!("  resource {r}\n")),
                    None => out.push_str("  resource (none named)\n"),
                }
                out.push_str(&format!("  asked by {}\n", s("subject")));
                if let Some(c) = p["caller_claims"].as_str() {
                    out.push_str(&format!("  claims  {c:?}  (its words, not a fact)\n"));
                }
                out.push('\n');
            }
            out.push_str(&format!(
                "{} waiting. Approve with: otwono-permctl approve <ID>\n",
                items.len()
            ));
            out
        }
        "request" | "claim" => match v["token"].as_str() {
            Some(t) => format!("token {t}\n"),
            None => format!("{v}\n"),
        },
        "approve" | "deny" => format!(
            "{} {}\n",
            v["confirmation_id"].as_str().unwrap_or(""),
            v["state"].as_str().unwrap_or("")
        ),
        "audit" => format!(
            "{} record(s), chain {}\n",
            v["records"].as_u64().unwrap_or(0),
            if v["intact"].as_bool().unwrap_or(false) {
                "intact"
            } else {
                "BROKEN"
            }
        ),
        _ => format!("{v}\n"),
    }
}

fn next_path<'a>(it: &mut impl Iterator<Item = &'a String>, flag: &str) -> Result<PathBuf, Error> {
    it.next()
        .map(PathBuf::from)
        .ok_or_else(|| Error::Usage(format!("{flag} needs a path")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn approve_without_an_id_is_a_usage_error_not_a_guess() {
        // Guessing here would mean approving something the person did not name.
        assert!(matches!(run(&opts(&["approve"])), Err(Error::Usage(_))));
        assert!(matches!(run(&opts(&["deny"])), Err(Error::Usage(_))));
    }

    #[test]
    fn request_without_an_action_is_a_usage_error() {
        assert!(matches!(run(&opts(&["request"])), Err(Error::Usage(_))));
        assert!(matches!(run(&opts(&["claim"])), Err(Error::Usage(_))));
    }

    #[test]
    fn an_unknown_command_is_refused() {
        assert!(matches!(
            run(&opts(&["confirm-everything"])),
            Err(Error::Usage(_))
        ));
    }

    #[test]
    fn help_needs_no_daemon() {
        assert!(run(&opts(&["--help"]))
            .unwrap()
            .contains("only a subject the node"));
    }

    #[test]
    fn the_usage_says_the_callers_reason_is_not_a_fact() {
        // The one thing a person reading this screen must not be misled about.
        let help = run(&opts(&["--help"])).unwrap();
        assert!(help.contains("assertion, not"), "{help}");
        assert!(help.contains("Read the resource"), "{help}");
    }

    #[test]
    fn listing_shows_the_resource_and_labels_the_claim() {
        let v = json!({
            "pending": [{
                "confirmation_id": "abc",
                "subject": "uid:1000",
                "action": "fs.delete",
                "summary": "Delete user data",
                "blast_radius": "Irreversible",
                "resource": "/home/u/tax-2025.ods",
                "caller_claims": "tidying up",
                "age_ms": 10,
            }],
        });
        let out = render("list", &v);
        assert!(out.contains("/home/u/tax-2025.ods"), "{out}");
        assert!(out.contains("Irreversible"), "{out}");
        assert!(out.contains("not a fact"), "{out}");
    }

    #[test]
    fn an_empty_list_says_so_rather_than_printing_nothing() {
        // A silent CLI reads as a broken one, and a person checking whether anything is
        // waiting deserves an answer either way.
        assert!(render("list", &json!({ "pending": [] })).contains("nothing is waiting"));
    }
}
