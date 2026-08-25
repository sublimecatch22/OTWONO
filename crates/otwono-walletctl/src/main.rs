//! `otwono-walletctl` — create a wallet, see what it is, and derive addresses.
//!
//! The wallet daemon had no client, exactly as the confirmation channel did not. This is
//! that client.
//!
//! It also carries the two-step shape ADR-0024 imposes on anything that needs a person.
//! `create` asks for a capability it cannot be given unattended: the first run prints the
//! confirmation id and stops, and the second run — after somebody has approved — claims the
//! token and proceeds. That is deliberately visible rather than hidden behind a spinner: a
//! command that appeared to hang while waiting for a human would be a worse lie than one
//! that says what it is waiting for.

#![forbid(unsafe_code)]

use otwono_proto::Client;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
otwono-walletctl — the household's wallet

USAGE:
    otwono-walletctl <COMMAND> [OPTIONS]

COMMANDS:
    status                    Whether a wallet exists here, and what it is
    create                    Create one. Needs a person to confirm, and shows the recovery
                              phrase once
    address                   Derive public keys. Needs the passphrase
    export-seed               Reveal the seed. This is the whole wallet

OPTIONS:
    --passphrase-stdin        Read the passphrase from stdin instead of prompting. For
                              scripts. There is deliberately no way to pass one as an
                              argument: it would land in shell history and in
                              /proc/<pid>/cmdline, where anything running as this user or as
                              root can read it
    --coin <N>                BIP-44 coin type (default 60)
    --account <N>             BIP-44 account (default 0)
    --index <N>               Repeatable. Which addresses to derive (default 0)
    --confirmation <ID>       Resume a command after somebody approved it
    --socket <PATH>           Wallet socket (default $OTWONO_SOCKET_DIR/wallet.sock)
    --perm-socket <PATH>      Permission broker (default $OTWONO_SOCKET_DIR/perm.sock)
    --json                    Print the daemon's reply verbatim
    -h, --help                Show this message

ABOUT THE PASSPHRASE:
    Prompted for, without echo, when this is run at a terminal. It is never accepted as a
    command-line argument, and it is asked for twice when creating a wallet — a typo in a
    passphrase nobody has written down yet is a wallet lost before it is used.

ABOUT THE RECOVERY PHRASE:
    `create` prints 24 words once. Write them down, off this machine. They are the only way
    back if the passphrase is forgotten, anyone who reads them owns the wallet, and nobody
    can recover them for you.

ABOUT ADDRESSES:
    A fresh one per counterparty and per purpose. Reusing a single address makes the
    household's whole history publicly linkable to anyone who ever sees one payment. Nothing
    here is shown without the passphrase, because this node stores no public key in the
    clear.

EXIT CODES:
    0  done
    1  usage error
    2  the daemon refused, or could not be reached
    3  waiting for somebody to confirm
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(out) => {
            print!("{out}");
            ExitCode::SUCCESS
        }
        Err(Error::Usage(m)) => {
            eprintln!("otwono-walletctl: {m}\n\n{USAGE}");
            ExitCode::from(1)
        }
        Err(Error::Runtime(m)) => {
            eprintln!("otwono-walletctl: {m}");
            ExitCode::from(2)
        }
        Err(Error::NeedsConfirmation(m)) => {
            // Its own exit code, so a script can tell "a person must act" from "this failed".
            print!("{m}");
            ExitCode::from(3)
        }
    }
}

#[derive(Debug)]
enum Error {
    Usage(String),
    Runtime(String),
    NeedsConfirmation(String),
}

struct Options {
    command: String,
    passphrase_stdin: bool,
    coin: u32,
    account: u32,
    indices: Vec<u32>,
    confirmation: Option<String>,
    socket: PathBuf,
    perm_socket: PathBuf,
    json: bool,
}

fn parse(args: &[String]) -> Result<Options, Error> {
    let mut command: Option<String> = None;
    let mut passphrase_stdin = false;
    let mut coin = 60u32;
    let mut account = 0u32;
    let mut indices: Vec<u32> = Vec::new();
    let mut confirmation = None;
    let mut socket: Option<PathBuf> = None;
    let mut perm_socket: Option<PathBuf> = None;
    let mut json = false;

    let num = |v: &str, flag: &str| -> Result<u32, Error> {
        v.parse()
            .map_err(|_| Error::Usage(format!("{flag} needs a number, not {v:?}")))
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut next = |flag: &str| -> Result<String, Error> {
            it.next()
                .cloned()
                .ok_or_else(|| Error::Usage(format!("{flag} needs a value")))
        };
        match arg.as_str() {
            "-h" | "--help" => return Err(Error::Usage("help".into())),
            "--json" => json = true,
            "--passphrase-stdin" => passphrase_stdin = true,
            "--passphrase" => {
                return Err(Error::Usage(
                    "--passphrase is not accepted: an argument lands in shell history and in \
                     /proc/<pid>/cmdline. Run this at a terminal to be prompted, or use \
                     --passphrase-stdin"
                        .into(),
                ))
            }
            "--coin" => coin = num(&next("--coin")?, "--coin")?,
            "--account" => account = num(&next("--account")?, "--account")?,
            "--index" => indices.push(num(&next("--index")?, "--index")?),
            "--confirmation" => confirmation = Some(next("--confirmation")?),
            "--socket" => socket = Some(next("--socket")?.into()),
            "--perm-socket" => perm_socket = Some(next("--perm-socket")?.into()),
            other if other.starts_with('-') => return Err(Error::Usage(format!("unknown option {other}"))),
            other if command.is_none() => command = Some(other.to_string()),
            other => return Err(Error::Usage(format!("unexpected argument {other}"))),
        }
    }
    if indices.is_empty() {
        indices.push(0);
    }
    Ok(Options {
        command: command.ok_or_else(|| Error::Usage("no command given".into()))?,
        passphrase_stdin,
        coin,
        account,
        indices,
        confirmation,
        socket: socket.unwrap_or_else(|| otwono_proto::socket_path("wallet")),
        perm_socket: perm_socket.unwrap_or_else(|| otwono_proto::socket_path("perm")),
        json,
    })
}

/// The capability each command needs, and whether a person must confirm it.
fn capability(command: &str) -> Result<(&'static str, bool), Error> {
    match command {
        "status" | "address" => Ok(("wallet.read", false)),
        "create" => Ok(("wallet.create", true)),
        "export-seed" => Ok(("wallet.export_seed", true)),
        other => Err(Error::Usage(format!("unknown command {other:?}"))),
    }
}

fn run(args: &[String]) -> Result<String, Error> {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        return Ok(USAGE.to_string());
    }
    let o = parse(args)?;
    let (action, confirms) = capability(&o.command)?;

    // Asked for twice when creating, once otherwise. A typo in a passphrase nobody has
    // written down yet is a wallet lost before it is used; a typo when opening one just
    // fails to open it, and asking twice there would be noise.
    let need_passphrase =
        |o: &Options| -> Result<String, Error> { read_passphrase(o.passphrase_stdin, o.command == "create") };
    let (method, params) = match o.command.as_str() {
        "status" => ("wallet.status", json!({})),
        "address" => (
            "wallet.public_keys",
            json!({
                "passphrase": need_passphrase(&o)?,
                "coin": o.coin,
                "account": o.account,
                "indices": o.indices,
            }),
        ),
        "create" => ("wallet.create", json!({ "passphrase": need_passphrase(&o)? })),
        "export-seed" => (
            "wallet.export_seed",
            json!({ "passphrase": need_passphrase(&o)? }),
        ),
        other => return Err(Error::Usage(format!("unknown command {other:?}"))),
    };

    let token = obtain_token(&o, action, confirms)?;
    let value = Client::connect(&o.socket)
        .map_err(|e| Error::Runtime(format!("cannot reach {}: {e}", o.socket.display())))?
        .call_with_capability(method, params, &token)
        .map_err(|e| Error::Runtime(format!("{method} transport failure: {e}")))?
        .map_err(|e| Error::Runtime(format!("{method} refused: {}", e.message)))?;

    if o.json {
        return Ok(format!("{value}\n"));
    }
    Ok(render(&o.command, &value))
}

/// Get a capability token, going through a confirmation when one is required.
fn obtain_token(o: &Options, action: &str, confirms: bool) -> Result<String, Error> {
    let mut broker = Client::connect(&o.perm_socket)
        .map_err(|e| Error::Runtime(format!("cannot reach {}: {e}", o.perm_socket.display())))?;

    // Resuming: somebody approved, and this run collects what that authorised.
    if let Some(id) = &o.confirmation {
        let v = broker
            .call("perm.claim", json!({ "confirmation_id": id }))
            .map_err(|e| Error::Runtime(format!("perm.claim transport failure: {e}")))?
            .map_err(|e| Error::Runtime(format!("perm.claim refused: {}", e.message)))?;
        return v["token"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| Error::Runtime("perm.claim returned no token".into()));
    }

    let reply = broker
        .call("perm.request", json!({ "action": action }))
        .map_err(|e| Error::Runtime(format!("perm.request transport failure: {e}")))?;
    match reply {
        Ok(v) => v["token"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| Error::Runtime("perm.request returned no token".into())),
        Err(e) if confirms => {
            // The two-step shape, made visible. A person has to act, and the message says
            // exactly what they must do rather than leaving them to read an ADR.
            let id = e
                .message
                .split("Confirmation ")
                .nth(1)
                .and_then(|t| t.split_whitespace().next())
                .unwrap_or("<id>");
            Err(Error::NeedsConfirmation(format!(
                "{action} needs somebody to confirm it.\n\
                 \n\
                   waiting  {id}\n\
                 \n\
                 Have a confirmer approve it:\n\
                 \n\
                   otwono-permctl list\n\
                   otwono-permctl approve {id}\n\
                 \n\
                 then run this again with --confirmation {id}\n"
            )))
        }
        Err(e) => Err(Error::Runtime(format!("policy refuses {action}: {}", e.message))),
    }
}

/// Obtain the passphrase, from stdin or from the terminal — never from an argument.
///
/// An argument would be visible in shell history and in `/proc/<pid>/cmdline` to anything
/// running as this user or as root, which for the key that holds a household's money is not
/// a trade worth making for convenience.
fn read_passphrase(from_stdin: bool, twice: bool) -> Result<String, Error> {
    if from_stdin {
        let mut line = String::new();
        std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut line)
            .map_err(|e| Error::Runtime(format!("cannot read the passphrase: {e}")))?;
        return passphrase_from_line(&line);
    }
    let first = rpassword::prompt_password("passphrase: ")
        .map_err(|e| Error::Runtime(format!("cannot prompt for a passphrase: {e}")))?;
    if first.is_empty() {
        return Err(Error::Usage("an empty passphrase is not accepted".into()));
    }
    if twice {
        let again = rpassword::prompt_password("passphrase again: ")
            .map_err(|e| Error::Runtime(format!("cannot prompt for a passphrase: {e}")))?;
        if again != first {
            return Err(Error::Usage("those did not match. Nothing was created".into()));
        }
    }
    Ok(first)
}

/// Turn one line of input into a passphrase.
///
/// Split out so the real thing is what the tests exercise: a test that reimplemented this
/// would be checking a copy, and the interesting behaviour — that trailing newlines go and
/// nothing else does — is exactly what a copy would get subtly wrong.
///
/// Only the line ending is removed. A passphrase is somebody's words, and trimming spaces
/// would silently change the key that opens their wallet.
fn passphrase_from_line(line: &str) -> Result<String, Error> {
    let line = line.trim_end_matches(['\n', '\r']).to_string();
    if line.is_empty() {
        return Err(Error::Usage("the passphrase on stdin was empty".into()));
    }
    Ok(line)
}

fn render(command: &str, v: &Value) -> String {
    let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
    match command {
        "status" => {
            if !v["exists"].as_bool().unwrap_or(false) {
                return format!("no wallet at {}\n{}\n", s("path"), s("note"));
            }
            format!(
                "wallet   {}\nvault    version {}, {} over {}\nkdf cost m={} t={} p={}\n{}\n",
                s("path"),
                v["version"].as_u64().unwrap_or(0),
                s("cipher"),
                s("kdf"),
                v["m_cost"].as_u64().unwrap_or(0),
                v["t_cost"].as_u64().unwrap_or(0),
                v["p_cost"].as_u64().unwrap_or(0),
                s("note"),
            )
        }
        "create" => format!(
            "wallet created at {}\n\n\
             Write these 24 words down now, off this machine:\n\n  {}\n\n{}\n",
            s("path"),
            s("recovery_phrase"),
            s("note"),
        ),
        "address" => {
            let mut out = String::new();
            for k in v["keys"].as_array().cloned().unwrap_or_default() {
                out.push_str(&format!(
                    "{}  {}\n",
                    k["path"].as_str().unwrap_or(""),
                    k["public_key"].as_str().unwrap_or("")
                ));
            }
            out.push_str(
                "\nThese are public keys, not addresses: which chain is not decided, and an\n\
                 address string is chain-specific.\n",
            );
            out
        }
        "export-seed" => format!("{}\n\n{}\n", s("seed_hex"), s("note"),),
        _ => format!("{v}\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn each_command_asks_for_the_capability_it_needs() {
        assert_eq!(capability("status").unwrap(), ("wallet.read", false));
        assert_eq!(capability("address").unwrap(), ("wallet.read", false));
        // The three that must stop for a person.
        assert_eq!(capability("create").unwrap(), ("wallet.create", true));
        assert_eq!(capability("export-seed").unwrap(), ("wallet.export_seed", true));
    }

    #[test]
    fn reading_never_asks_for_a_capability_that_confirms() {
        // If this inverted, showing a balance would demand a person and the pressure would
        // be to widen something that must not widen.
        for c in ["status", "address"] {
            assert!(!capability(c).unwrap().1, "{c} should not need confirming");
        }
    }

    #[test]
    fn a_passphrase_is_never_accepted_as_an_argument() {
        // The whole point: an argument lands in shell history and in /proc/<pid>/cmdline.
        // Refused with an explanation rather than silently ignored, so somebody who has been
        // scripting it finds out why rather than wondering where their passphrase went.
        match parse(&opts(&["create", "--passphrase", "hunter2"])) {
            Err(Error::Usage(m)) => {
                assert!(m.contains("shell history"), "{m}");
                assert!(m.contains("--passphrase-stdin"), "{m}");
            }
            Err(e) => panic!("--passphrase should be refused as a usage error, got {e:?}"),
            Ok(_) => panic!("--passphrase was accepted"),
        }
    }

    #[test]
    fn an_empty_passphrase_from_stdin_is_refused() {
        // Reading an empty line and carrying on would encrypt a wallet under nothing.
        assert!(matches!(passphrase_from_line(""), Err(Error::Usage(_))));
        assert!(matches!(passphrase_from_line("\n"), Err(Error::Usage(_))));
    }

    #[test]
    fn a_passphrase_from_stdin_keeps_its_spaces_and_loses_its_newline() {
        // A passphrase is somebody's words. Trimming spaces would silently change the key.
        assert_eq!(passphrase_from_line("  two words  \n").unwrap(), "  two words  ");
        assert_eq!(passphrase_from_line("pass\r\n").unwrap(), "pass");
    }

    #[test]
    fn an_unknown_command_is_refused() {
        assert!(matches!(capability("drain"), Err(Error::Usage(_))));
    }

    #[test]
    fn indices_default_to_one_and_are_repeatable() {
        assert_eq!(parse(&opts(&["address"])).unwrap().indices, vec![0]);
        let o = parse(&opts(&["address", "--index", "3", "--index", "7"])).unwrap();
        assert_eq!(o.indices, vec![3, 7]);
    }

    #[test]
    fn the_usage_says_the_phrase_is_shown_once_and_nobody_can_recover_it() {
        let help = run(&opts(&["--help"])).unwrap();
        assert!(help.contains("nobody\n    can recover them"), "{help}");
        assert!(help.contains("fresh one per counterparty"), "{help}");
    }

    #[test]
    fn creating_prints_the_phrase_and_the_warning_together() {
        // They must not be separable: a phrase shown without the warning invites somebody
        // to assume it can be looked up again.
        let out = render(
            "create",
            &json!({
                "path": "/var/lib/otwono/wallet/seed.vault",
                "recovery_phrase": "abandon ability able about above absent absorb abstract",
                "note": "write these down now",
            }),
        );
        assert!(out.contains("abandon ability"), "{out}");
        assert!(out.contains("Write these 24 words down"), "{out}");
        assert!(out.contains("write these down now"), "{out}");
    }

    #[test]
    fn addresses_are_labelled_as_public_keys_rather_than_addresses() {
        // The chain is not decided, so calling these addresses would be deciding it in a
        // help string.
        let out = render(
            "address",
            &json!({ "keys": [{ "path": "m/44'/60'/0'/0/0", "public_key": "02ab" }] }),
        );
        assert!(out.contains("public keys, not addresses"), "{out}");
    }
}
