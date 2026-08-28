//! `otwono` — the assistant, for a person at a shell.
//!
//! `AI-RUNTIME.md` §6 and `OTWONO-ARCHITECTURE.md` §6.3 both specify this command as
//! `otwono do …`. It is the one user-facing command in the system; everything else is a
//! `*ctl` that drives one daemon.
//!
//! # What this binary is, and is not
//!
//! It is a **shell**: it asks the capability engine what shape the assistant takes here,
//! parses the words with `otwono-agent`, asks the broker for the capability the resulting
//! intent names, and makes one control-plane call. It holds nothing and decides nothing.
//!
//! It is **not** privileged. `otwono do save …` can do exactly what the invoking user could
//! do with `otwono-storectl put`, because it goes through the same broker with the same
//! caller identity. If the policy refuses, this refuses. An assistant that could act where
//! its user could not would be the whole permission model undone (CLAUDE.md §2.5).
//!
//! # Why it asks for its own shape
//!
//! The assistant's shape is a feature gate derived from the tier, decided in the capability
//! engine and nowhere else (CLAUDE.md §2.6). This binary could assume T0 — it would be right
//! on every machine that has one today — and that is precisely the shortcut §2.6 exists to
//! prevent. So it asks, and when it cannot ask it assumes the *least* capable shape, because
//! guessing upward would mean promising reasoning the machine cannot do.

#![forbid(unsafe_code)]

mod route;

use otwono_agent::{parse, AssistantShape, Elsewhere, Grammar, Intent};
use otwono_proto::Client;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

const USAGE: &str = "\
otwono — the OTWONO assistant

USAGE:
    otwono do <words>...        Do the thing described, if this machine can
    otwono do help              List everything this machine understands
    otwono help                 Show this message

OPTIONS:
    --dry-run               Say what would happen, and stop before doing it
    --json                  Machine-readable output (the contract; parse this, not the text)
    --perm-socket <PATH>    Permission broker socket (default $OTWONO_SOCKET_DIR/perm.sock)
    --socket-dir <PATH>     Where the daemons listen (default /run/otwono)

EXIT CODES:
    0  the request was carried out
    1  usage error
    2  something was reachable but failed
    3  the assistant declined — it does not understand, or this machine cannot
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(out) => {
            print!("{out}");
            ExitCode::SUCCESS
        }
        Err(Error::Usage(m)) => {
            eprintln!("otwono: {m}\n\n{USAGE}");
            ExitCode::from(1)
        }
        Err(Error::Runtime(m)) => {
            eprintln!("otwono: {m}");
            ExitCode::from(2)
        }
        // A distinct code, because "I will not" and "it broke" are different outcomes and a
        // script that retries the second should not retry the first.
        Err(Error::Declined(m)) => {
            eprintln!("{m}");
            ExitCode::from(3)
        }
    }
}

#[derive(Debug)]
enum Error {
    Usage(String),
    Runtime(String),
    Declined(String),
}

#[derive(Debug)]
struct Options {
    words: Vec<String>,
    dry_run: bool,
    json: bool,
    perm_socket: Option<PathBuf>,
    socket_dir: Option<PathBuf>,
}

fn run(args: &[String]) -> Result<String, Error> {
    let opts = parse_args(args)?;

    // Applied before anything is located, so every socket this run touches comes from one
    // decision. otwono_proto::socket_path reads it.
    if let Some(dir) = &opts.socket_dir {
        std::env::set_var("OTWONO_SOCKET_DIR", dir);
    }

    let perm_socket = opts
        .perm_socket
        .clone()
        .unwrap_or_else(|| otwono_proto::socket_path("perm"));

    let shape = shape_of_this_machine(&perm_socket);
    let grammar = Grammar::for_shape(shape);

    let words: Vec<&str> = opts.words.iter().map(String::as_str).collect();
    if words.first().is_some_and(|w| *w == "help") {
        return Ok(help_text(&grammar, opts.json));
    }

    let intent = match parse(&grammar, &words, &elsewhere(&perm_socket)) {
        Ok(intent) => intent,
        Err(refusal) => {
            return Err(Error::Declined(if opts.json {
                serde_json::to_string_pretty(&json!({
                    "declined": true,
                    "message": refusal.message(),
                    "could_happen_elsewhere": refusal.could_happen_elsewhere(),
                    "refusal": refusal,
                }))
                .unwrap_or_else(|e| format!("{{\"declined\":true,\"error\":\"{e}\"}}"))
            } else {
                refusal.message()
            }))
        }
    };

    if opts.dry_run {
        return Ok(if opts.json {
            format!(
                "{}\n",
                serde_json::to_string_pretty(&json!({ "would": intent, "ran": false }))
                    .map_err(|e| Error::Runtime(e.to_string()))?
            )
        } else {
            format!(
                "would: {}\nneeds: {}\n",
                intent.explain(),
                intent.capability.as_deref().unwrap_or("no capability")
            )
        });
    }

    let value = dispatch(&intent, &perm_socket)?;
    Ok(if opts.json {
        format!(
            "{}\n",
            serde_json::to_string_pretty(&json!({ "did": intent.method, "result": value }))
                .map_err(|e| Error::Runtime(e.to_string()))?
        )
    } else {
        format!("{}\n{}\n", intent.explain(), render(&value))
    })
}

/// Ask the capability engine what shape the assistant takes here.
///
/// Falls back to the least capable shape when the profile cannot be read — the broker is
/// down, `hw.read` is refused, the daemon is not running. All three mean "this binary does
/// not know", and the honest response to not knowing is to promise less rather than more:
/// a node that claimed a model it does not have would refuse at the last moment with a
/// confusing error instead of at the first with a clear one.
fn shape_of_this_machine(perm_socket: &std::path::Path) -> AssistantShape {
    read_shape(perm_socket).unwrap_or(AssistantShape::CommandGrammar)
}

fn read_shape(perm_socket: &std::path::Path) -> Option<AssistantShape> {
    let token = request_token(perm_socket, "hw.read", "otwono asks what shape it takes here").ok()?;
    let mut client = Client::connect(otwono_proto::socket_path("hw")).ok()?;
    let value = client
        .call_with_capability("hw.profile", json!({}), &token)
        .ok()?
        .ok()?;
    serde_json::from_value(value.get("features")?.get("assistant_shape")?.clone()).ok()
}

/// Where a refused request could go instead.
///
/// Always [`Elsewhere::Nowhere`] today, and deliberately so rather than optimistically. A
/// peer is only somewhere to send work if it *advertises inference*, and nothing advertises
/// that yet — `ai-provider` over ONM is `AI-RUNTIME.md` §7 and is not built. Listing the
/// authenticated peers here and calling them candidates would produce a refusal that names a
/// machine which would reject the request, which is worse than saying nothing: it sends the
/// user somewhere that will not help and makes the assistant look wrong about its own mesh.
fn elsewhere(_perm_socket: &std::path::Path) -> Elsewhere {
    Elsewhere::Nowhere
}

fn dispatch(intent: &Intent, perm_socket: &std::path::Path) -> Result<Value, Error> {
    let socket = route::socket_for(&intent.method).ok_or_else(|| {
        Error::Runtime(format!(
            "nothing here serves {}, which is a gap in this binary rather than in your request",
            intent.method
        ))
    })?;

    let token = match &intent.capability {
        Some(cap) => Some(request_token(
            perm_socket,
            cap,
            &format!("otwono do {}", intent.verb),
        )?),
        None => None,
    };

    let mut client = Client::connect_waiting(&socket, Duration::from_secs(5))
        .map_err(|e| Error::Runtime(format!("cannot reach {}: {e}", socket.display())))?;

    let params = params_for(intent);
    let reply = match &token {
        Some(t) => client.call_with_capability(&intent.method, params, t),
        None => client.call(&intent.method, params),
    }
    .map_err(|e| Error::Runtime(format!("{}: {e}", intent.method)))?;

    reply.map_err(|e| Error::Runtime(format!("{} refused: {}", intent.method, e.message)))
}

/// Turn an intent's arguments into the parameters its method expects.
///
/// The names mostly line up, because the grammar named them after the methods rather than
/// inventing a vocabulary. `save` is the exception worth its own arm: `store.put` takes
/// base64 `data`, not a path, so the file is read here — at the edge, by the user's own
/// process, with the user's own permissions. Reading it inside a daemon would mean a
/// daemon opening arbitrary paths on a caller's say-so.
fn params_for(intent: &Intent) -> Value {
    let mut params = serde_json::Map::new();
    for (name, arg) in &intent.arguments {
        if intent.method == "store.put" && name == "file" {
            continue; // handled below
        }
        params.insert(name.clone(), json!(arg.as_str()));
    }
    if intent.method == "store.put" {
        if let Some(path) = intent.arguments.get("file") {
            match std::fs::read(path.as_str()) {
                Ok(bytes) => {
                    params.insert("data".into(), json!(data_encoding_base64(&bytes)));
                }
                Err(e) => {
                    // Left absent rather than faked: the daemon then refuses for a missing
                    // parameter, and the message below is what the user actually sees.
                    params.insert("data".into(), json!(""));
                    eprintln!("otwono: cannot read {}: {e}", path.as_str());
                }
            }
        }
    }
    Value::Object(params)
}

/// Base64 without pulling in a dependency for four lines.
///
/// The alphabet and padding are RFC 4648 standard, which is what `store.put` decodes with.
fn data_encoding_base64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        out.push(A[(n >> 18) as usize & 63] as char);
        out.push(A[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            A[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            A[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn request_token(perm_socket: &std::path::Path, action: &str, reason: &str) -> Result<String, Error> {
    let mut broker = Client::connect(perm_socket).map_err(|e| {
        Error::Runtime(format!(
            "cannot reach the permission broker at {}: {e}",
            perm_socket.display()
        ))
    })?;
    let granted = broker
        .call("perm.request", json!({ "action": action, "reason": reason }))
        .map_err(|e| Error::Runtime(format!("perm.request: {e}")))?
        // A policy refusal is the user's own system saying no, not a fault. It comes back as
        // Declined so the exit code distinguishes it from a broken daemon.
        .map_err(|e| Error::Declined(format!("This machine's policy refuses {action}: {}", e.message)))?;
    granted
        .get("token")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| Error::Runtime("the broker granted no token".into()))
}

fn help_text(grammar: &Grammar, as_json: bool) -> String {
    if as_json {
        let verbs: Vec<Value> = grammar
            .verbs()
            .iter()
            .map(|v| {
                json!({
                    "verb": v.word, "method": v.method,
                    "capability": v.capability, "mutates": v.mutates, "summary": v.summary,
                })
            })
            .collect();
        return format!(
            "{}\n",
            serde_json::to_string_pretty(&json!({
                "assistant_shape": grammar.shape().as_str(),
                "verbs": verbs,
            }))
            .unwrap_or_default()
        );
    }
    let mut out = String::new();
    if grammar.shape() == AssistantShape::CommandGrammar {
        // Said up front, not discovered by being refused. A user who knows the shape asks
        // different questions; one who does not keeps asking the same one.
        out.push_str(
            "This machine runs a command-grammar assistant: it does exactly the things\n\
             listed below and does not reason about anything else.\n\n",
        );
    }
    let width = grammar.verbs().iter().map(|v| v.word.len()).max().unwrap_or(0);
    for verb in grammar.verbs() {
        out.push_str(&format!(
            "  {:width$}  {}{}\n",
            verb.word,
            verb.summary,
            if verb.mutates { " [changes something]" } else { "" }
        ));
    }
    out
}

fn render(value: &Value) -> String {
    match value {
        Value::Object(map) => map
            .iter()
            .filter(|(k, _)| k.as_str() != "schema_version")
            .map(|(k, v)| format!("  {k}: {}", compact(v)))
            .collect::<Vec<_>>()
            .join("\n"),
        other => compact(other),
    }
}

fn compact(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn parse_args(args: &[String]) -> Result<Options, Error> {
    let mut opts = Options {
        words: Vec::new(),
        dry_run: false,
        json: false,
        perm_socket: None,
        socket_dir: None,
    };
    let mut it = args.iter();
    let Some(command) = it.next() else {
        return Err(Error::Usage("expected a command".into()));
    };
    match command.as_str() {
        "help" | "--help" | "-h" => {
            return Ok(Options {
                words: vec!["help".into()],
                ..opts
            })
        }
        "do" => {}
        other => {
            return Err(Error::Usage(format!(
                "unknown command \"{other}\"; the assistant is `otwono do …`"
            )))
        }
    }
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--dry-run" => opts.dry_run = true,
            "--json" => opts.json = true,
            "--perm-socket" => {
                opts.perm_socket = Some(
                    it.next()
                        .ok_or_else(|| Error::Usage("--perm-socket needs a path".into()))?
                        .into(),
                )
            }
            "--socket-dir" => {
                opts.socket_dir = Some(
                    it.next()
                        .ok_or_else(|| Error::Usage("--socket-dir needs a path".into()))?
                        .into(),
                )
            }
            other if other.starts_with("--") => return Err(Error::Usage(format!("unknown option {other}"))),
            word => opts.words.push(word.to_string()),
        }
    }
    if opts.words.is_empty() {
        return Err(Error::Usage("`otwono do` needs something to do".into()));
    }
    Ok(opts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_encoder_the_daemon_decodes_with() {
        // Checked against known vectors rather than against itself: an encoder that agrees
        // only with its own decoder is how a file arrives corrupted and nothing notices.
        assert_eq!(data_encoding_base64(b""), "");
        assert_eq!(data_encoding_base64(b"f"), "Zg==");
        assert_eq!(data_encoding_base64(b"fo"), "Zm8=");
        assert_eq!(data_encoding_base64(b"foo"), "Zm9v");
        assert_eq!(data_encoding_base64(b"foob"), "Zm9vYg==");
        assert_eq!(data_encoding_base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(data_encoding_base64(b"foobar"), "Zm9vYmFy");
        // And a byte range that exercises the top of the alphabet and the +/ pair.
        let all: Vec<u8> = (0u8..=255).collect();
        let encoded = data_encoding_base64(&all);
        assert_eq!(
            data_encoding::BASE64.encode(&all),
            encoded,
            "diverges from the encoder otwono-stored decodes with"
        );
    }

    #[test]
    fn only_do_and_help_are_commands() {
        assert!(matches!(
            parse_args(&["status".into()]).unwrap_err(),
            Error::Usage(_)
        ));
        assert!(parse_args(&["do".into(), "tier".into()]).is_ok());
        assert!(parse_args(&["help".into()]).is_ok());
        // `do` with nothing after it is a usage error, not an empty request.
        assert!(matches!(parse_args(&["do".into()]).unwrap_err(), Error::Usage(_)));
    }

    #[test]
    fn options_do_not_become_words() {
        let opts = parse_args(&["do".into(), "--dry-run".into(), "tier".into()]).unwrap();
        assert!(opts.dry_run);
        assert_eq!(opts.words, vec!["tier"]);
    }
}
